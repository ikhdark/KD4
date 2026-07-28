Audit only `investigation_cases/lifecycle_cancelled_child_leak.rs` for a
cancellation or lifecycle defect. Do not edit files and do not inspect
unrelated repository surfaces.

Trace both `tokio::select!` branches and the spawned child's terminal state.
Report findings first. For each confirmed finding, identify the violated
invariant, reachable causal path, practical consequence, and exact file and
symbol locator. Report plausible but unconfirmed issues separately with the
exact missing fact.
