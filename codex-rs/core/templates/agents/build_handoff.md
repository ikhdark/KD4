# Reusable build handoff

One build owner need not be root. Keep authorized builds with the execution worker when cohesive; serialize only conflicting source/build resources. If another owner must build, the consuming worker waits in-turn for the artifact instead of returning for phase approval.

Hand off the exact immutable bundle/receipt, source and build-settings identity, and supported workload. Recover existing receipts or live handles after a lost response; do not duplicate a build or infer completion from an artifact path. Rebuild only for a concrete mismatch. Before transferring the resource, settle outstanding commands: an interrupted turn or sent release is not proof that a compiler or benchmark has stopped. A reusable binary does not certify unrelated source or workload correctness.
