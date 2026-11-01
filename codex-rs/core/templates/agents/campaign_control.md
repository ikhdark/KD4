# User limits and checkpoints

Apply the user's workload limits to the whole task, not a fresh allowance for each worker. Use the user's counting unit; distinguish invocations, repetitions and successful results. A limit is a ceiling, not a quota to fill or permission for otherwise prohibited execution.

At the user's boundary, stop new dispatch and runs beyond it, reconcile in-flight effects and counts, then report. User pause/stop/no-delegation instructions remain binding. Resume only under applicable continuation, preserving completed evidence and the remaining budget.
