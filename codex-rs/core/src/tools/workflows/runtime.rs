use super::parser::WorkflowMeta;
use super::store::JournalRecord;
use serde_json::Value;

pub(super) struct RuntimeInput<'a> {
    pub(super) run_id: &'a str,
    pub(super) script_path: &'a str,
    pub(super) body: &'a str,
    pub(super) meta: &'a WorkflowMeta,
    pub(super) args: &'a Value,
    pub(super) journal: &'a [Option<JournalRecord>],
    pub(super) concurrency: usize,
}

pub(super) fn build(input: RuntimeInput<'_>) -> Result<String, String> {
    let run_id = json(input.run_id)?;
    let script_path = json(input.script_path)?;
    let body = json(input.body)?;
    let workflow_name = json(&input.meta.name)?;
    let phases = json(
        &input
            .meta
            .phases
            .iter()
            .map(|phase| phase.title.as_str())
            .collect::<Vec<_>>(),
    )?;
    let args = json(input.args)?;
    let journal = json(input.journal)?;
    let concurrency = input.concurrency.max(1);
    let concurrency = concurrency.to_string();

    interpolate([
        ("__CODEX_RUN_ID__", run_id.as_str()),
        ("__CODEX_SCRIPT_PATH__", script_path.as_str()),
        ("__CODEX_WORKFLOW_NAME__", workflow_name.as_str()),
        ("__CODEX_WORKFLOW_BODY__", body.as_str()),
        ("__CODEX_PHASES__", phases.as_str()),
        ("__CODEX_ARGS__", args.as_str()),
        ("__CODEX_JOURNAL__", journal.as_str()),
        ("__CODEX_CONCURRENCY__", concurrency.as_str()),
    ])
}

fn json<T>(value: &T) -> Result<String, String>
where
    T: serde::Serialize + ?Sized,
{
    serde_json::to_string(value).map_err(|err| format!("failed to build workflow runtime: {err}"))
}

fn interpolate<const N: usize>(replacements: [(&str, &str); N]) -> Result<String, String> {
    let mut source = WORKFLOW_RUNTIME;
    let mut output = String::with_capacity(WORKFLOW_RUNTIME.len());
    while let Some((offset, marker, value)) = replacements
        .iter()
        .filter_map(|(marker, value)| source.find(marker).map(|offset| (offset, *marker, *value)))
        .min_by_key(|(offset, _, _)| *offset)
    {
        output.push_str(&source[..offset]);
        output.push_str(value);
        source = &source[offset + marker.len()..];
    }
    output.push_str(source);
    Ok(output)
}

const WORKFLOW_RUNTIME: &str = r#"
const __workflowRunId = __CODEX_RUN_ID__;
const __workflowScriptPath = __CODEX_SCRIPT_PATH__;
const __workflowName = __CODEX_WORKFLOW_NAME__;
const __workflowBody = __CODEX_WORKFLOW_BODY__;
const __workflowPhases = __CODEX_PHASES__;
const __workflowArgs = __CODEX_ARGS__;
const __workflowJournal = __CODEX_JOURNAL__;
const __workflowConcurrency = __CODEX_CONCURRENCY__;

const __hostTool = name => {
  const exact = ALL_TOOLS.find(tool => tool.name === name);
  const matches = exact ? [exact] : ALL_TOOLS.filter(tool => tool.name.endsWith(`__${name}`));
  if (matches.length !== 1 || typeof tools[matches[0].name] !== 'function') {
    throw new Error(`workflow runtime requires exactly one ${name} tool`);
  }
  return tools[matches[0].name];
};
const __spawnAgent = __hostTool('spawn_agent');
const __listAgents = __hostTool('list_agents');
const __followupTask = __hostTool('followup_task');
const __appendJournal = __hostTool('workflow_journal_append');
const __loadWorkflow = __hostTool('workflow_load');

const __OriginalDate = Date;
globalThis.Date = class DeterministicWorkflowDate extends __OriginalDate {
  constructor(...values) {
    if (!values.length) throw new Error('argless new Date() is not deterministic; pass a timestamp through args');
    super(...values);
  }
  static now() { throw new Error('Date.now() is not deterministic; pass a timestamp through args'); }
};
Math.random = () => { throw new Error('Math.random() is not deterministic; vary work by agent index'); };

const __sleep = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));
const __stableKey = value => JSON.stringify(value, (_key, item) => {
  if (!item || typeof item !== 'object' || Array.isArray(item)) return item;
  return Object.fromEntries(Object.keys(item).sort().map(key => [key, item[key]]));
});
const __hash = value => {
  let hash = 2166136261;
  for (let index = 0; index < value.length; index++) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 16777619);
  }
  return (hash >>> 0).toString(36);
};

let __activeAgents = 0;
const __agentWaiters = [];
const __withAgentSlot = async operation => {
  if (__activeAgents >= __workflowConcurrency) {
    await new Promise(resolve => __agentWaiters.push(resolve));
  }
  __activeAgents++;
  try {
    return await operation();
  } finally {
    __activeAgents--;
    __agentWaiters.shift()?.();
  }
};

let __agentIndex = 0;
let __resumePrefixOpen = true;
const __terminal = status => {
  if (status && typeof status === 'object' && Object.hasOwn(status, 'completed')) {
    return { done: true, value: status.completed ?? '' };
  }
  if (status && typeof status === 'object' && Object.hasOwn(status, 'errored')) {
    return { done: true, value: null };
  }
  if (['shutdown', 'not_found'].includes(status)) return { done: true, value: null };
  return { done: false, value: null };
};
const __waitForAgent = async (taskName, previousResult = undefined) => {
  let sawNewTurn = previousResult === undefined;
  for (let poll = 0; poll < 172800; poll++) {
    const result = await __listAgents({ path_prefix: taskName });
    const agent = result.agents?.find(item => item.agent_name === taskName);
    if (!agent) return null;
    const terminal = __terminal(agent.agent_status);
    if (!terminal.done) sawNewTurn = true;
    if (terminal.done && (sawNewTurn || terminal.value !== previousResult)) return terminal.value;
    await __sleep(500);
  }
  return null;
};

const __schemaMatches = (schema, value) => {
  if (!schema || typeof schema !== 'object') return true;
  if (Object.hasOwn(schema, 'const') && !Object.is(schema.const, value)) return false;
  if (Array.isArray(schema.enum) && !schema.enum.some(item => __stableKey(item) === __stableKey(value))) return false;
  const types = Array.isArray(schema.type) ? schema.type : schema.type ? [schema.type] : [];
  if (types.length) {
    const actual = value === null ? 'null'
      : Array.isArray(value) ? 'array'
      : Number.isInteger(value) ? 'integer'
      : typeof value === 'number' ? 'number'
      : typeof value;
    if (!types.includes(actual) && !(actual === 'integer' && types.includes('number'))) return false;
  }
  if (Array.isArray(schema.anyOf) && !schema.anyOf.some(item => __schemaMatches(item, value))) return false;
  if (Array.isArray(schema.oneOf) && schema.oneOf.filter(item => __schemaMatches(item, value)).length !== 1) return false;
  if (Array.isArray(schema.allOf) && !schema.allOf.every(item => __schemaMatches(item, value))) return false;
  if (Array.isArray(value) && schema.items && !value.every(item => __schemaMatches(schema.items, item))) return false;
  if (value && typeof value === 'object' && !Array.isArray(value)) {
    if (Array.isArray(schema.required) && !schema.required.every(key => Object.hasOwn(value, key))) return false;
    if (schema.properties && !Object.entries(schema.properties).every(([key, child]) =>
      !Object.hasOwn(value, key) || __schemaMatches(child, value[key]))) return false;
    if (schema.additionalProperties === false && schema.properties &&
        !Object.keys(value).every(key => Object.hasOwn(schema.properties, key))) return false;
  }
  return true;
};
const __structured = (text, schema) => {
  try {
    const value = JSON.parse(text);
    return __schemaMatches(schema, value) ? value : null;
  } catch (_error) {
    return null;
  }
};

const agent = (prompt, options = {}) => {
  const index = __agentIndex++;
  if (index >= 1000) return Promise.resolve(null);
  if (typeof prompt !== 'string' || !prompt.trim()) throw new Error('agent() requires a non-empty prompt');
  if (!options || typeof options !== 'object' || Array.isArray(options)) throw new Error('agent() options must be an object');
  if (options.isolation !== undefined && options.isolation !== 'shared') {
    throw new Error('workflow worktree isolation is not supported yet; use shared isolation or omit it');
  }
  const key = __stableKey({ prompt, options });
  const cached = __workflowJournal[index];
  if (__resumePrefixOpen && cached?.key === key) return Promise.resolve(cached.value);
  __resumePrefixOpen = false;

  return __withAgentSlot(async () => {
    const label = String(options.label ?? `agent_${index}`)
      .toLowerCase()
      .replace(/[^a-z0-9_]+/g, '_')
      .replace(/^_+|_+$/g, '')
      .slice(0, 24) || `agent_${index}`;
    const taskName = `wf_${__workflowRunId.replaceAll('-', '').slice(0, 8)}_${index}_${label}_${__hash(key)}`;
    const structuredSuffix = options.schema
      ? `\n\nReturn only JSON matching this schema exactly:\n${JSON.stringify(options.schema)}`
      : '';
    const spawnArgs = {
      message: prompt + structuredSuffix,
      task_name: taskName,
      fork_turns: 'none',
    };
    if (options.agentType !== undefined) spawnArgs.agent_type = options.agentType;
    if (options.model !== undefined) spawnArgs.model = options.model;
    if (options.effort !== undefined) spawnArgs.reasoning_effort = options.effort;
    let value = null;
    try {
      const spawned = await __spawnAgent(spawnArgs);
      const target = spawned.task_name ?? taskName;
      const textResult = await __waitForAgent(target);
      if (textResult !== null && options.schema) {
        value = __structured(textResult, options.schema);
        if (value === null) {
          await __followupTask({
            target,
            message: `Your previous answer did not match the required JSON schema. Return only corrected JSON matching: ${JSON.stringify(options.schema)}`,
          });
          const corrected = await __waitForAgent(target, textResult);
          value = corrected === null ? null : __structured(corrected, options.schema);
        }
      } else {
        value = textResult;
      }
    } catch (_error) {
      value = null;
    }
    await __appendJournal({ runId: __workflowRunId, index, key, value });
    return value;
  });
};

const parallel = thunks => {
  if (!Array.isArray(thunks) || thunks.length > 4096) throw new Error('parallel() expects at most 4096 thunks');
  return Promise.all(thunks.map(thunk => Promise.resolve().then(thunk).catch(() => null)));
};
const pipeline = (items, ...stages) => {
  if (!Array.isArray(items) || items.length > 4096) throw new Error('pipeline() expects at most 4096 items');
  if (!stages.every(stage => typeof stage === 'function')) throw new Error('pipeline() stages must be functions');
  return Promise.all(items.map((original, index) => (async () => {
    let current = original;
    for (const stage of stages) {
      if (current === null) break;
      try { current = await stage(current, original, index); }
      catch (_error) { current = null; }
    }
    return current;
  })()));
};

const budget = Object.freeze({
  total: null,
  spent: () => 0,
  remaining: () => Infinity,
});
const __AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
const __executeBody = async (body, bodyArgs, phaseTitles, depth) => {
  const declared = new Set(phaseTitles);
  const phase = title => {
    if (!declared.has(title)) throw new Error(`phase(${JSON.stringify(title)}) is not declared exactly in meta.phases`);
    notify(`Workflow ${__workflowName} — ${title}`);
  };
  const log = message => notify(`Workflow ${__workflowName}: ${String(message)}`);
  const localAgent = (prompt, options = {}) => {
    if (options.phase !== undefined && !declared.has(options.phase)) {
      throw new Error(`agent phase ${JSON.stringify(options.phase)} is not declared exactly in meta.phases`);
    }
    return agent(prompt, options);
  };
  const nestedWorkflow = async (target, nestedArgs = null) => {
    if (depth >= 1) throw new Error('nested workflows are limited to one level');
    const selector = typeof target === 'string' ? { name: target } : target;
    if (!selector || typeof selector !== 'object') throw new Error('workflow() expects a saved name or {scriptPath}');
    const loaded = await __loadWorkflow(selector);
    return __executeBody(loaded.body, nestedArgs, loaded.meta.phases.map(item => item.title), depth + 1);
  };
  const fn = new __AsyncFunction('agent', 'parallel', 'pipeline', 'phase', 'log', 'args', 'budget', 'workflow', body);
  return fn(localAgent, parallel, pipeline, phase, log, bodyArgs, budget, nestedWorkflow);
};

text(JSON.stringify({ taskId: __workflowRunId, scriptPath: __workflowScriptPath, name: __workflowName }));
yield_control();
try {
  const result = await __executeBody(__workflowBody, __workflowArgs, __workflowPhases, 0);
  notify(`Workflow ${__workflowName} completed${result === undefined ? '' : `: ${JSON.stringify(result)}`}`);
} catch (error) {
  notify(`Workflow ${__workflowName} failed: ${error?.message ?? String(error)}`);
  throw error;
}
"#;

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
