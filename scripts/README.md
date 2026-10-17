# Repository scripts

Scripts and their tests live together by owner:

| Directory | Area |
| --- | --- |
| `benchmark_scripts/` | Compatibility helpers and non-audit regression fixtures still required by KD4 tooling; audit scripts moved to KDR |
| `build/` | Rust runners, Cargo lanes, build diagnostics, and validation metrics |
| `codex_package/` | Package construction, archives, and npm staging |
| `common/` | Shared atomic I/O, process ownership, generation locks, and tool versions |
| `install/` | Installation and local publishing |
| `maintenance/` | Repository checks, formatting, source inventory, and Python test routing |
| `schemas/` | Schema/protobuf validation helpers |

The shared Python project and its lockfile remain here. Run commands from the
repository root; direct Python entrypoints also resolve imports from their own
location rather than the caller's working directory.

```powershell
python -B scripts/maintenance/root_maintenance.py test-python --module scripts.build.test_rust_tool_env
python -B scripts/maintenance/root_maintenance.py test-python --changed scripts/build/rust_tool_env.py
python -I -B scripts/build/rust_test_runner.py list-targets
```

The `kd4-perf-snapshot` benchmark recipe was retired. Other `just` and package-manager recipe names are unchanged. Import helpers
through their area, for example `from scripts.common import atomic_json`.
Keep the area `__init__.py` files: standard unittest discovery needs them to
recurse into the reorganized folders. Use the maintenance runner above for
cross-environment execution, including Harbor's separate interpreter.
Keep reusable scripts and regression tests. Run saved-session audits from KDR;
live/build experiment launchers have been retired. The remaining compatibility
modules preserve the imports used by validation ledgers, maintenance, and oracle
tests; they do not provide benchmark launchers. Do not remove these shared helpers.

Historical machine-readable audit results retain their original recorded paths
and hashes; they are observations, not current invocation instructions.
