# Campaign budgets and checkpoints

For a user-limited campaign, root owns the shared total and nonoverlapping worker reservations; each worker retains its attempted/qualified counts and remaining allowance. Root's own runs use the same pool. Apply the user's counting unit; distinguish invocations, repetitions and successful results. Allocate enough for the bounded experiment and required comparison once, not approval per invocation. A reservation is a ceiling, not a quota to fill or permission for otherwise prohibited execution.

Bind grants to the current scope and checkpoint. At the user's boundary, stop new dispatch and runs beyond it, reconcile in-flight effects and counts, then report. User pause/stop/no-delegation instructions supersede outstanding grants; a resource release does not restart the campaign. Resume only under applicable continuation, preserving completed evidence and the remaining budget.
