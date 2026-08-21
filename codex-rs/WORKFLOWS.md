# JavaScript workflows

This fork can run deterministic JavaScript programs that coordinate Codex subagents in the
background. Enable the experimental feature and restart Codex:

```toml
[features]
workflows = true
```

Workflows run only after explicit opt-in: ask Codex to use a workflow or fan out agents, or request
a saved workflow by name. The `workflow` tool immediately returns a task ID and persisted script
path; progress and completion arrive as notifications. Use `/workflows` to open the live agent
view.

Every script is plain JavaScript and starts with a pure-literal `meta` export:

```js
export const meta = {
  name: 'find-flaky-tests',
  description: 'Find flaky tests and propose fixes',
  whenToUse: 'When CI is intermittently failing',
  phases: [
    { title: 'Scan', detail: 'Find retry evidence' },
    { title: 'Verify', detail: 'Check each candidate' },
  ],
}

phase('Scan')
const candidates = await agent('Find tests with retry evidence.', {
  schema: {
    type: 'object',
    properties: { tests: { type: 'array', items: { type: 'string' } } },
    required: ['tests'],
    additionalProperties: false,
  },
})
phase('Verify')
const results = await parallel(candidates.tests.map(candidate => () =>
  agent(`Verify ${candidate}`, { phase: 'Verify' })
))
```

The runtime provides `agent`, `parallel`, `pipeline`, `phase`, `log`, `args`, `budget`, and
one-level `workflow` composition. `agent` accepts `label`, `phase`, `schema`, `model`, `effort`,
and `agentType`; schema results are parsed and validated before being returned. `pipeline` runs
each item through all stages independently, while `parallel` is a barrier.

Saved workflows are loaded from the project's `.codex/workflows/<name>.js` first, then from
`$CODEX_HOME/workflows/<name>.js`. Each invocation is copied into the session's workflow run
directory. Re-run an edited returned path with `scriptPath`; include `resumeFromRunId` to replay
the longest unchanged prefix of journaled `agent()` calls.

The runtime has no Node, filesystem, or network APIs. `Date.now()`, `Math.random()`, and argless
`new Date()` are rejected so resumable control flow remains deterministic. Pass timestamps and
other varying inputs through `args`. A workflow is capped at 1,000 lifetime agents, each
`parallel` or `pipeline` call accepts at most 4,096 items, and concurrent agents are capped at
`min(16, CPUs - 2)` with a minimum of one.
