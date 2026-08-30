Fix the demonstrated readiness race in
`investigation_cases/repair_ready_signal.py`. The type already has an explicit
readiness state and notification path.

Make the smallest state-based change, edit only the implementation file, and
do not edit the test or add files. Do not introduce waiting or timing behavior.
Run `investigation_cases/test_repair_ready_signal.py`, then give a concise final
answer.
