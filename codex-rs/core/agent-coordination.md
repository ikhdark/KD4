# Coordinate scoped agent work

These are opt-in workflows over existing native tools. Use them only when
delegation is authorized and keep the root's task and evidence obligations
separate from worker status. None of these workflows starts a background
scheduler, transfers authority, or adds independent evidence by copying it.

## Use one ownership view

Give each assignment a stable task name and an explicit read/write scope.
KD4 registers admitted assignments and current attempt bindings automatically.
Keep the original spawn outcomes, including refusals and uncertain calls.
When reconciling several admitted assignments, fetch one compact
`agents.agent_team` list rather than reopening every known attempt:

```js
const list = await resolve_tool("agents.agent_team")({
  operation: "list", arguments: {compact: true}
});
text(list.typed_tasks.map(({task_name, assignment_id, attempt_id, workspace_id}) =>
  ({task_name, assignment_id, attempt_id, workspace_id})));
```

Join the view to the exact assignment and attempt IDs, then retrieve a single
task's full scope or receipt if that decision needs it. Missing bindings and
unresolved descendants are not successful admissions. Do not replay a
partially accepted spawn to reconstruct the view.

## Reconcile exact findings; keep detail recoverable

Have workers publish scoped findings with source identities, hashes, exact
locations, and limitations. After collecting immutable report references
(`assignment_id`, `attempt_id`, `version`), group them in one cell:

```js
const task = resolve_tool("agents.agent_task");
const refs = load("findingReportRefs"); // exact, nonempty selections
const packet = await task({operation: "group_findings", arguments: {results: refs}});
if (packet.acceptance !== "not_evaluated" || packet.discards_reports !== false)
  throw Error("Unexpected grouping receipt");
store("groupedFindingReports", packet);
text({
  reports: packet.reports.map(({assignment_id, attempt_id, version, result}) =>
    ({assignment_id, attempt_id, version, summary: result.summary, limitations: result.limitations})),
  groups: packet.groups,
  separate_verification: packet.verification_candidates
});
```

Use one current evidence check for an exact group when its members depend on
the same source observation. Keep original report IDs and counts; do not
combine conflicts, changed sources, or noncurrent attempt authority into a
verified finding. Grouping does not alter receipts or accept work. When more
detail is needed, read the exact reference with
`agents.agent_task({operation:"partial_result", arguments:ref})` and check its
assignment, attempt, and version. Project the decision and evidence locators
after inspecting the detail in the cell. When code mode prints an unchanged
native grouping result, KD4 replaces large, byte-identical repeated evidence
with an `evidence_same_as` JSON pointer to its first inline copy. Distinct,
short, edited, and ineligible data remains inline. JavaScript receives the full
result, and exact partial-result lookup remains available for each report.

## Route evidence and progress separately

Send identical evidence only to the explicit recipients who need it. Include
the actionable discovery, its source or exact recovery reference, and the
constraint it changes; do not forward the sender's history. Before sending a
reference, make sure the recipient can read it. For worker-to-root evidence,
use a typed partial result or the normal completion receipt; this workflow
does not grant ancestor messaging.

```js
const send = resolve_tool("agents.send_message");
const delivery = await send({targets: load("evidenceRecipients"), message: load("evidenceNote")});
text({status: delivery.status, accepted: delivery.accepted_targets, failed: delivery.failed_targets});
```

Queue acceptance is not consumption and does not start a turn. Reconcile
partial outcomes per recipient; do not resend to accepted targets. If the
work requires a new turn, use an explicitly authorized follow-up.

KD4 automatically coalesces safe queued revisions when the producer identifies
progress that is useful and entirely replaceable. Send `progress:
{task_id, attempt_id, revision}` with stable task and attempt IDs and
monotonically increasing revisions. A new attempt has its own ID. Leave off
this metadata for findings, blockers, errors, decisions, handoffs, and any
transition a receiver must see. Keep ordinary messages as ordering barriers.
This compacts queued progress only; it cannot retract consumed messages.

## Dispatch only after evidence is ready

For a root's typed spawn with one live, current typed prerequisite in the
same workspace, KD4 can wait up to three seconds for a durable receipt change
before reserving a slot or creating the downstream assignment. The original
admission check still decides whether the receipt, current source, and
permissions allow launch. No queued job survives a cancellation or restart.
If the dependency is not eligible or does not become ready in time, the
normal admission/refusal path applies. For longer waits or multiple
prerequisites, use selected completion waits and exact applicable receipts
before sending the downstream request. Do not replay an uncertain dispatch.

Declare what independent observations and judgments are required **before**
dispatch. Where the contract permits, stop requesting more reviews when all
of those requirements already have applicable exact receipts. Distinct agents
or attempt IDs are not enough: copies, aliases, a shared underlying
observation, conflicts, and missing or stale proof do not satisfy an
independent requirement. Run the review when required; never use this rule
to waive an explicit reviewer, verifier, or gate.

Assign final synthesis once, normally to the coordinating root. Ask workers
for their scoped conclusions, evidence, limitations, and unresolved work.
The owner reads the exact receipts and integrates those contributions into
one final report. Keep separate reviews and user-requested independent
reports; omit only duplicate drafts of that same synthesis.
