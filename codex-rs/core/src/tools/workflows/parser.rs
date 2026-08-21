use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Number;
use serde_json::Value;

const META_PREFIX: &str = "export const meta";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct WorkflowMeta {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) when_to_use: Option<String>,
    pub(super) phases: Vec<WorkflowPhase>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkflowPhase {
    pub(super) title: String,
    pub(super) detail: String,
}

#[derive(Debug)]
pub(super) struct ParsedWorkflow<'a> {
    pub(super) meta: WorkflowMeta,
    pub(super) body: &'a str,
}

pub(super) fn parse(source: &str) -> Result<ParsedWorkflow<'_>, String> {
    let trimmed = source.trim_start();
    let leading = source.len() - trimmed.len();
    let after_declaration = trimmed
        .strip_prefix(META_PREFIX)
        .ok_or_else(|| "workflow must start with `export const meta = { ... }`".to_string())?;
    let declaration_whitespace = after_declaration.len() - after_declaration.trim_start().len();
    let after_equals = after_declaration
        .trim_start()
        .strip_prefix('=')
        .ok_or_else(|| "workflow meta declaration must assign a literal object".to_string())?;
    let whitespace_after_equals = after_equals.len() - after_equals.trim_start().len();
    let literal_start =
        leading + META_PREFIX.len() + declaration_whitespace + 1 + whitespace_after_equals;
    let literal_source = after_equals.trim_start();
    let mut parser = LiteralParser::new(literal_source);
    let value = parser.parse_value()?;
    parser.skip_trivia()?;
    let literal_end = literal_start + parser.position();
    let mut body_start = literal_end;
    if source[body_start..].starts_with(';') {
        body_start += 1;
    }
    let meta = serde_json::from_value::<WorkflowMeta>(value)
        .map_err(|err| format!("invalid workflow meta: {err}"))?;
    validate_meta(&meta)?;
    Ok(ParsedWorkflow {
        meta,
        body: &source[body_start..],
    })
}

fn validate_meta(meta: &WorkflowMeta) -> Result<(), String> {
    if meta.name.trim().is_empty() || meta.name.len() > 100 {
        return Err("workflow meta.name must contain 1 to 100 characters".to_string());
    }
    if meta.description.trim().is_empty() || meta.description.len() > 500 {
        return Err("workflow meta.description must contain 1 to 500 characters".to_string());
    }
    if meta.phases.len() > 100 {
        return Err("workflow meta.phases cannot contain more than 100 phases".to_string());
    }
    let mut titles = std::collections::HashSet::new();
    for phase in &meta.phases {
        if phase.title.trim().is_empty() || phase.title.len() > 100 {
            return Err("workflow phase titles must contain 1 to 100 characters".to_string());
        }
        if phase.detail.len() > 500 {
            return Err("workflow phase details cannot exceed 500 characters".to_string());
        }
        if !titles.insert(phase.title.as_str()) {
            return Err(format!(
                "workflow phase title {:?} is declared more than once",
                phase.title
            ));
        }
    }
    Ok(())
}

struct LiteralParser<'a> {
    source: &'a str,
    position: usize,
}

impl<'a> LiteralParser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            position: 0,
        }
    }

    fn position(&self) -> usize {
        self.position
    }

    fn parse_value(&mut self) -> Result<Value, String> {
        self.skip_trivia()?;
        match self.peek_char() {
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('"' | '\'') => self.parse_string().map(Value::String),
            Some('-' | '0'..='9') => self.parse_number().map(Value::Number),
            Some(_) if self.consume_keyword("true") => Ok(Value::Bool(true)),
            Some(_) if self.consume_keyword("false") => Ok(Value::Bool(false)),
            Some(_) if self.consume_keyword("null") => Ok(Value::Null),
            Some(character) => Err(format!(
                "workflow meta must be a pure literal; unexpected {character:?}"
            )),
            None => Err("workflow meta literal is missing".to_string()),
        }
    }

    fn parse_object(&mut self) -> Result<Value, String> {
        self.expect_char('{')?;
        let mut object = Map::new();
        loop {
            self.skip_trivia()?;
            if self.consume_char('}') {
                break;
            }
            let key = match self.peek_char() {
                Some('"' | '\'') => self.parse_string()?,
                Some(character) if is_identifier_start(character) => self.parse_identifier(),
                _ => return Err("workflow meta object keys must be identifiers or strings".into()),
            };
            self.skip_trivia()?;
            self.expect_char(':')?;
            let value = self.parse_value()?;
            if object.insert(key.clone(), value).is_some() {
                return Err(format!("workflow meta contains duplicate key {key:?}"));
            }
            self.skip_trivia()?;
            if self.consume_char('}') {
                break;
            }
            self.expect_char(',')?;
            self.skip_trivia()?;
            if self.consume_char('}') {
                break;
            }
        }
        Ok(Value::Object(object))
    }

    fn parse_array(&mut self) -> Result<Value, String> {
        self.expect_char('[')?;
        let mut values = Vec::new();
        loop {
            self.skip_trivia()?;
            if self.consume_char(']') {
                break;
            }
            values.push(self.parse_value()?);
            self.skip_trivia()?;
            if self.consume_char(']') {
                break;
            }
            self.expect_char(',')?;
            self.skip_trivia()?;
            if self.consume_char(']') {
                break;
            }
        }
        Ok(Value::Array(values))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        let quote = self
            .next_char()
            .ok_or_else(|| "unterminated workflow meta string".to_string())?;
        let mut output = String::new();
        loop {
            let character = self
                .next_char()
                .ok_or_else(|| "unterminated workflow meta string".to_string())?;
            if character == quote {
                return Ok(output);
            }
            if character != '\\' {
                output.push(character);
                continue;
            }
            let escaped = self
                .next_char()
                .ok_or_else(|| "unterminated workflow meta escape".to_string())?;
            match escaped {
                '\\' | '/' | '"' | '\'' => output.push(escaped),
                'b' => output.push('\u{0008}'),
                'f' => output.push('\u{000c}'),
                'n' => output.push('\n'),
                'r' => output.push('\r'),
                't' => output.push('\t'),
                'u' => {
                    let digits = self.take_ascii(4)?;
                    let code = u32::from_str_radix(digits, 16)
                        .map_err(|_| "invalid workflow meta unicode escape".to_string())?;
                    let character = char::from_u32(code)
                        .ok_or_else(|| "invalid workflow meta unicode escape".to_string())?;
                    output.push(character);
                }
                _ => return Err(format!("unsupported workflow meta escape \\{escaped}")),
            }
        }
    }

    fn parse_number(&mut self) -> Result<Number, String> {
        let start = self.position;
        while self
            .peek_char()
            .is_some_and(|character| matches!(character, '-' | '+' | '.' | 'e' | 'E' | '0'..='9'))
        {
            self.next_char();
        }
        serde_json::from_str::<Number>(&self.source[start..self.position])
            .map_err(|_| "invalid workflow meta number".to_string())
    }

    fn parse_identifier(&mut self) -> String {
        let start = self.position;
        self.next_char();
        while self.peek_char().is_some_and(is_identifier_continue) {
            self.next_char();
        }
        self.source[start..self.position].to_string()
    }

    fn consume_keyword(&mut self, keyword: &str) -> bool {
        if !self.source[self.position..].starts_with(keyword) {
            return false;
        }
        let end = self.position + keyword.len();
        if self.source[end..]
            .chars()
            .next()
            .is_some_and(is_identifier_continue)
        {
            return false;
        }
        self.position = end;
        true
    }

    fn take_ascii(&mut self, count: usize) -> Result<&'a str, String> {
        let end = self.position.saturating_add(count);
        let value = self
            .source
            .get(self.position..end)
            .ok_or_else(|| "unterminated workflow meta escape".to_string())?;
        if !value.is_ascii() {
            return Err("invalid workflow meta escape".to_string());
        }
        self.position = end;
        Ok(value)
    }

    fn expect_char(&mut self, expected: char) -> Result<(), String> {
        if self.consume_char(expected) {
            Ok(())
        } else {
            Err(format!("expected {expected:?} in workflow meta literal"))
        }
    }

    fn consume_char(&mut self, expected: char) -> bool {
        if self.peek_char() == Some(expected) {
            self.next_char();
            true
        } else {
            false
        }
    }

    fn skip_trivia(&mut self) -> Result<(), String> {
        loop {
            while self.peek_char().is_some_and(char::is_whitespace) {
                self.next_char();
            }
            if self.source[self.position..].starts_with("//") {
                while self.peek_char().is_some_and(|character| character != '\n') {
                    self.next_char();
                }
                continue;
            }
            if self.source[self.position..].starts_with("/*") {
                let Some(end) = self.source[self.position + 2..].find("*/") else {
                    return Err("unterminated comment in workflow meta".to_string());
                };
                self.position += end + 4;
                continue;
            }
            return Ok(());
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.source[self.position..].chars().next()
    }

    fn next_char(&mut self) -> Option<char> {
        let character = self.peek_char()?;
        self.position += character.len_utf8();
        Some(character)
    }
}

fn is_identifier_start(character: char) -> bool {
    character.is_ascii_alphabetic() || matches!(character, '_' | '$')
}

fn is_identifier_continue(character: char) -> bool {
    is_identifier_start(character) || character.is_ascii_digit()
}

#[cfg(test)]
#[path = "parser_tests.rs"]
mod tests;
