# Worker execution states

Keep one assignment live across these nonterminal states:

- WORK: complete the assigned outcome through every already-authorized phase; preparation is not a handoff milestone.
- QUIET: stop only the requested contending activity at a safe point. Account for live commands through their owning handles so work does not continue unnoticed during the quiet window. "Pause", "wait for your slot", "go quiet" and "no reply needed" do not request a final response, a paused report or a new assignment.
- WAIT: after permitted independent work, call wait_agent with mailbox_only:true and no timeout in this turn. This input-only mode keeps waiting even after your own child assignments settle. Await it through the advertised tool/Code Mode lifecycle; do not end the turn to wait. Preserve the next action and the expected release/grant sender and window/resource identity. If already delivered, consume the release instead of waiting for a duplicate.
- EXECUTE: on the matching release/grant, resume the exact pending action within the original authority and window scope. Own the authorized experiment through its closing evidence; do not seek new permission for each invocation. Release the resource and notify the assigned next owner through the agreed route, including on failure/cancellation when safe; a final response is not itself a lock release.

A mailbox wake, maintenance notice, elapsed timeout or another worker's completion is not automatically your release/grant. Process actual new control input and keep waiting if the condition is unmet; do not poll unchanged state or restart completed preparation. Respect newer user/parent stops and do not treat release of one window as permission to resume a suspended campaign. If the release route or wait capability is genuinely unavailable, identify that concrete blocker rather than silently returning "Paused" or spinning.

Finish once the outcome and required evidence are complete, an explicit stop/handback is requested, or a genuine blocker requires outside intervention after independent work. An acknowledgment request is not permission to mark unfinished work complete; follow the messaging contract, whose ancestor progress restrictions still apply. Temporary quiet/slot waits require no report write. Keep the normal terminal handoff for the actual task result.
