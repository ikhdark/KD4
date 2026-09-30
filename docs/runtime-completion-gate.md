# Runtime task-completion gate

With `features.kd4_runtime` and `features.kd4_completion_gate` enabled
(both default on), a model-proposed completion with an active implementation
obligation is assessed before
the runtime finishes the turn. The gate does not depend on configured stop hooks.

Before sampling, a conservative local scope check derives the obligation from
authoritative user messages and trusted successful-completion receipts.
Explicit explanation/read-only requests bypass the assessor; questions, status
questions, and acknowledgements keep the previous obligation (so they cost no
assessor request unless implementation work is already unresolved). Ambiguous
requests remain gated. This is a bounded textual scope recognizer, not general
natural-language intent detection. Compacted history without a retained contract
is treated conservatively. Internal workers (review, compaction, agent jobs,
memory) are not gated; delegated agents are.

The assessor is one tool-free structured model request over the current
conversation, including the proposed final answer. It is not a spawned agent.
The runtime rejects an unresolved implementation/fix outcome when authorized
discriminating investigation, capture/instrumentation, implementation, or
validation remains available. Rejection returns to the existing tool-enabled
sampling loop. It does not authorize additional actions.

Completion requires either:

- an achieved outcome with evidence and a justified or independently validated
  fix; or
- a concrete user-input, authorization, or external-access dependency, with
  evidence that no useful authorized action remains.

Non-implementation requests retain their requested scope. The assessment follows
active obligations across turns; a status question is not cancellation.
Cancellation, explicit owner terminals, and the existing generation budget remain
host boundaries: without regular generation capacity the proposed answer is
delivered unassessed instead of being discarded. If the assessor request fails,
the answer is delivered with a warning that completion was not verified; a failed
side request never discards the working agent's answer. Existing user stop hooks
and finalizers still run after admission.

## Limits and cost

The semantic assessment is model-authored, not an independent proof that a fix is
correct or that a blocker is genuine. Rust enforces its structured decision table.
Mock tests prove runtime continuation and tool availability, not live-model
judgment. For gated requests, final-answer text (including legacy unphased
messages) is held before deltas, item lifecycle, raw-response publication, and
conversation persistence. Only a gate rejection discards it. Text followed by a
tool call in the same response did not end that response and is published in
provider order; held text is also published whenever the turn continues (tool
results, steering input, compaction). Admitted text uses the existing
finalized-text replay path before after-agent and stop hooks run, as without the
gate. Commentary and tool activity remain live.

This adds a model request, tokens, and latency only at gated completion boundaries.
Assessor usage counts toward total token usage, not the conversation's
context-window accounting. Malformed assessment JSON gets one tool-free retry; a
second malformed result rejects completion with generic continuation feedback once
per user input, and a repeated malformed assessment delivers the answer with a
warning instead of looping. It uses the configured model/provider and
counts against the existing no-progress generation budget. Set
`features.kd4_completion_gate = false` to disable this check independently of the
other KD4 controls. No user configuration is changed by the source patch.

The shared scripted test fixtures (core, the app-server mock config writers, and
the exec harness) disable the assessment by default so unrelated
transport/request-count tests keep testing their original contracts. Dedicated
completion-gate tests enable it and script both working and assessment requests.

Rebuilding/replacing the installed binary and restarting Desktop are separate
activation steps; editing these sources does not activate the gate.
