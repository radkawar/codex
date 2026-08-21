use super::parser;
use super::parser::ParsedWorkflow;
use crate::session::turn_context::TurnContext;
use codex_protocol::ThreadId;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) const MAX_WORKFLOW_SOURCE_BYTES: usize = 64 * 1024;
const MAX_WORKFLOW_AGENTS: u32 = 1000;
const MAX_JOURNAL_RECORD_BYTES: usize = 1024 * 1024;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct JournalRecord {
    pub(super) index: u32,
    pub(super) key: String,
    pub(super) value: Value,
}

pub(super) fn run_root(turn: &TurnContext, thread_id: ThreadId) -> PathBuf {
    turn.config
        .codex_home
        .join("workflow-runs")
        .join(thread_id.to_string())
        .into_path_buf()
}

pub(super) fn journal_path(
    turn: &TurnContext,
    thread_id: ThreadId,
    run_id: &str,
) -> Result<PathBuf, String> {
    validate_component(run_id, "run ID")?;
    Ok(run_root(turn, thread_id).join(format!("{run_id}.journal.jsonl")))
}

pub(super) async fn persist_script(path: PathBuf, source: String) -> Result<(), String> {
    if source.len() > MAX_WORKFLOW_SOURCE_BYTES {
        return Err(format!(
            "workflow source exceeds the {MAX_WORKFLOW_SOURCE_BYTES}-byte limit"
        ));
    }
    tokio::task::spawn_blocking(move || atomic_write(&path, source.as_bytes()))
        .await
        .map_err(|err| format!("workflow script writer failed: {err}"))?
}

pub(super) async fn load_source(
    turn: &TurnContext,
    thread_id: ThreadId,
    name: Option<&str>,
    script_path: Option<&str>,
) -> Result<(String, PathBuf), String> {
    match (name, script_path) {
        (Some(name), None) => {
            validate_component(name, "workflow name")?;
            let file_name = format!("{name}.js");
            let project_path = turn
                .config
                .cwd
                .join(".codex")
                .join("workflows")
                .join(&file_name);
            let global_path = turn.config.codex_home.join("workflows").join(file_name);
            let path = if tokio::fs::try_exists(&project_path).await.unwrap_or(false) {
                project_path.into_path_buf()
            } else {
                global_path.into_path_buf()
            };
            read_source(path).await
        }
        (None, Some(script_path)) => {
            let path = PathBuf::from(script_path);
            let canonical = tokio::fs::canonicalize(&path).await.map_err(|err| {
                format!("failed to resolve workflow path {}: {err}", path.display())
            })?;
            let allowed_roots = [
                run_root(turn, thread_id),
                turn.config
                    .cwd
                    .join(".codex")
                    .join("workflows")
                    .into_path_buf(),
                turn.config.codex_home.join("workflows").into_path_buf(),
            ];
            let mut allowed = false;
            for root in allowed_roots {
                if let Ok(root) = tokio::fs::canonicalize(root).await
                    && canonical.starts_with(root)
                {
                    allowed = true;
                    break;
                }
            }
            if !allowed {
                return Err("scriptPath must be a returned workflow path or live under `.codex/workflows` or `$CODEX_HOME/workflows`".to_string());
            }
            read_source(canonical).await
        }
        (Some(_), Some(_)) => Err("provide only one of name or scriptPath".to_string()),
        (None, None) => Err("provide name or scriptPath".to_string()),
    }
}

pub(super) fn parse_source(source: &str) -> Result<ParsedWorkflow<'_>, String> {
    if source.len() > MAX_WORKFLOW_SOURCE_BYTES {
        return Err(format!(
            "workflow source exceeds the {MAX_WORKFLOW_SOURCE_BYTES}-byte limit"
        ));
    }
    parser::parse(source)
}

pub(super) async fn load_journal(path: PathBuf) -> Result<Vec<Option<JournalRecord>>, String> {
    tokio::task::spawn_blocking(move || {
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(format!("failed to open workflow journal: {err}")),
        };
        let journal_bytes = file
            .metadata()
            .map_err(|err| format!("failed to inspect workflow journal: {err}"))?
            .len();
        if journal_bytes > MAX_JOURNAL_BYTES {
            return Err(format!(
                "workflow journal exceeds the {MAX_JOURNAL_BYTES}-byte limit"
            ));
        }
        let mut records = BTreeMap::new();
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|err| format!("failed to read workflow journal: {err}"))?;
            if line.len() > MAX_JOURNAL_RECORD_BYTES {
                return Err(format!(
                    "workflow journal record exceeds the {MAX_JOURNAL_RECORD_BYTES}-byte limit"
                ));
            }
            let record: JournalRecord = serde_json::from_str(&line)
                .map_err(|err| format!("workflow journal is invalid: {err}"))?;
            if record.index >= MAX_WORKFLOW_AGENTS {
                return Err(format!(
                    "workflow journal agent index must be below {MAX_WORKFLOW_AGENTS}"
                ));
            }
            records.insert(record.index, record);
        }
        let Some(max_index) = records.keys().next_back().copied() else {
            return Ok(Vec::new());
        };
        let mut output = vec![None; max_index as usize + 1];
        for (index, record) in records {
            output[index as usize] = Some(record);
        }
        Ok(output)
    })
    .await
    .map_err(|err| format!("workflow journal reader failed: {err}"))?
}

pub(super) async fn append_journal(path: PathBuf, record: JournalRecord) -> Result<(), String> {
    if record.index >= MAX_WORKFLOW_AGENTS {
        return Err(format!(
            "workflow journal agent index must be below {MAX_WORKFLOW_AGENTS}"
        ));
    }
    let serialized = serde_json::to_vec(&record)
        .map_err(|err| format!("failed to serialize workflow journal record: {err}"))?;
    if serialized.len() > MAX_JOURNAL_RECORD_BYTES {
        return Err(format!(
            "workflow journal record exceeds the {MAX_JOURNAL_RECORD_BYTES}-byte limit"
        ));
    }
    tokio::task::spawn_blocking(move || {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("failed to create workflow journal directory: {err}"))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|err| format!("failed to open workflow journal: {err}"))?;
        file.lock()
            .map_err(|err| format!("failed to lock workflow journal: {err}"))?;
        let journal_bytes = file
            .metadata()
            .map_err(|err| format!("failed to inspect workflow journal: {err}"))?
            .len();
        if journal_bytes.saturating_add(serialized.len() as u64 + 1) > MAX_JOURNAL_BYTES {
            return Err(format!(
                "workflow journal exceeds the {MAX_JOURNAL_BYTES}-byte limit"
            ));
        }
        file.write_all(&serialized)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_data())
            .map_err(|err| format!("failed to append workflow journal: {err}"))?;
        file.unlock()
            .map_err(|err| format!("failed to unlock workflow journal: {err}"))
    })
    .await
    .map_err(|err| format!("workflow journal writer failed: {err}"))?
}

fn validate_component(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(format!(
            "{label} must contain only ASCII letters, digits, `_`, or `-`"
        ));
    }
    Ok(())
}

async fn read_source(path: PathBuf) -> Result<(String, PathBuf), String> {
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|err| format!("failed to inspect workflow {}: {err}", path.display()))?;
    if metadata.len() > MAX_WORKFLOW_SOURCE_BYTES as u64 {
        return Err(format!(
            "workflow source exceeds the {MAX_WORKFLOW_SOURCE_BYTES}-byte limit"
        ));
    }
    let source = tokio::fs::read_to_string(&path)
        .await
        .map_err(|err| format!("failed to read workflow {}: {err}", path.display()))?;
    Ok((source, path))
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "workflow path has no parent directory".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("failed to create workflow directory: {err}"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|err| format!("failed to create temporary workflow file: {err}"))?;
    temp.write_all(contents)
        .and_then(|()| temp.flush())
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|err| format!("failed to write workflow file: {err}"))?;
    temp.persist(path)
        .map_err(|err| format!("failed to persist workflow file: {}", err.error))?;
    Ok(())
}

pub(super) async fn write_run_script(
    turn: Arc<TurnContext>,
    thread_id: ThreadId,
    run_id: &str,
    source: String,
) -> Result<PathBuf, String> {
    validate_component(run_id, "run ID")?;
    let path = run_root(&turn, thread_id).join(format!("{run_id}.js"));
    persist_script(path.clone(), source).await?;
    Ok(path)
}
