# Share one read-only investigation between consumers

This is the default Code Mode workflow for authorized batches of ordinary
read-only assignments. When request IDs need the **same answer from the same
source version**, give them the same message. The harness groups exact matches
within that wave without an alias key. It does not match similar wording or
reuse results across waves.

## Harness helpers

When available, use the built-in Code Mode helpers documented in the
[exec reference](../code-mode-protocol/src/description/exec_reference.md).
Start once, keeping the complete returned wave:

```javascript
const wave = await spawn_read_wave([
  {id: "first", message: "Read source.txt at the agreed revision and report its heading."},
  {id: "second", message: "Read source.txt at the agreed revision and report its heading."},
]);
store("readWaveAdmissions", wave);
text(wave.unresolved);
```

In a later cell, call `await collect_read_wave(load("readWaveAdmissions"))`.
Each request maps to a `report_index` in the returned `reports` array. Aliases
share the same index. Check receipt status and evidence before accepting;
a null index is unfinished or unavailable. The helper does not retry any
admission, even if it failed before dispatch. Review unresolved producers
and their native failures separately. You can collect again on the same
wave once pending work settles; do not reassign or follow up on its producers
as part of collection.

When early reports have useful independent work (for example, inspecting cited
evidence) and other workers may run much longer, pass `{completion_mode:"any"}`.
The helper then returns at the first terminal worker with a `remaining` wave;
store `remaining` and collect it in a later cell. Its rows map only the
producers that were still uncollected. The default (`"all"`) still returns once
every admitted worker settles.

Make the source version, scope, and evidence requirements explicit in the
message when they differ. For independent verification or separate owners,
use distinct `producer` names or direct assignments, even when the messages
are identical. Named producers are never merged with unnamed groups; repeating
a name with the exact same message still declares an alias. A shared report
is not a second independent proof. Use normal typed assignments when the
work needs a typed contract or write authority.

For two complementary questions about the same fixed source, use one
read-only assignment with both question IDs when one inspection can answer
both and neither answer requires independent verification. Ask the worker
to return each answer and its evidence separately. This is a dispatch
choice, not automatic matching of different messages by the helper.

## Older builds: manual equivalent

Store the current wave under `readWave`: rows with a unique `id`, a valid
agent task name `producer`, and a read-only assignment `message`. Give exact
aliases the same producer and message. Include the source revision in the
assignment when freshness matters. Start the following cell **once**:

```js
const rows = load("readWave"), messages = new Map();
if (!rows.length || new Set(rows.map(r => r.id)).size !== rows.length)
  throw Error("Expected a nonempty wave with distinct request IDs");
for (const {producer, message} of rows) {
  if (!producer || !message || (messages.has(producer) && messages.get(producer) !== message))
    throw Error("Conflicting or empty producer assignment");
  messages.set(producer, message);
}
const names = [...messages.keys()], spawn = resolve_tool("agents.spawn_agent");
const calls = await Promise.allSettled(names.map(task_name =>
  spawn({task_name, message: messages.get(task_name)})));
const admitted = [], unresolved = [];
for (const [i, call] of calls.entries()) {
  if (call.status === "fulfilled" && call.value?.assignment_id)
    admitted.push({producer: names[i], target: call.value.task_name, id: call.value.assignment_id});
  else unresolved.push({producer: names[i], outcome: String(call.reason ?? JSON.stringify(call.value))});
}
store("readWaveAdmissions", {rows, admitted, unresolved});
text({admitted, unresolved});
```

Do not replay the spawn cell on partial or uncertain admission. Reconcile
unresolved producers separately. Once ready to join, run:

```js
const {rows, admitted, unresolved} = load("readWaveAdmissions");
const wait = resolve_tool("agents.wait_agent"), get = resolve_tool("agents.get_agent_task");
const joined = admitted.length ? await wait({targets: admitted.map(a => a.target),
  completion_mode: "all", timeout_ms: 60000}) : null;
const reports = new Map();
for (const a of admitted) {
  const done = joined?.completion?.outcomes?.find(o => o.assignment_id === a.id);
  if (!done?.attempt_id) continue;
  const {receipt} = await get({assignment_id: a.id, attempt_id: done.attempt_id});
  if (receipt?.assignment_id === a.id && receipt.attempt_id === done.attempt_id)
    reports.set(a.producer, {attempt: receipt.attempt_id, status: receipt.status, summary: receipt.summary});
}
text({results: rows.map(({id, producer}) => ({id, producer, report: reports.get(producer) ?? null})),
  unresolved, pending: joined?.completion?.pending ?? []});
```

A null report is outstanding work. Inspect the identified receipt, underlying
evidence and current applicability before accepting any answer. If another
assignment depends on that evidence, wait for the applicable sealed receipt
before spawning it; do not use an early refused spawn as a readiness probe.
