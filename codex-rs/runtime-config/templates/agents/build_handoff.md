# Reusable build evidence

Reuse a build only when its immutable receipt, source, build settings and supported workload match. Recover existing receipts or live handles after a lost response; do not duplicate a build or infer completion from an artifact path. Rebuild only for a concrete mismatch. An interrupted turn is not proof that a compiler or benchmark has stopped. A reusable binary does not certify unrelated source or workload correctness.
