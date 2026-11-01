# Local npm packaging

Use `scripts/codex_package/stage_npm_packages.py` from the repository root to
stage local tarballs. This prepares packages; it does not publish them. Publishing
or republishing requires explicit permission.

For a prehydrated canonical vendor tree containing both supported Windows targets
(`x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc`):

```powershell
python .\scripts\codex_package\stage_npm_packages.py `
  --release-version 0.0.0-dev `
  --vendor-src C:\path\to\vendor `
  --package codex `
  --package codex-sdk
```

`--package codex` expands to the launcher plus Windows x64 and ARM64 native
packages. `codex-sdk` stages the TypeScript SDK with a matching CLI dependency.
The helper writes tarballs to `dist/npm/` by default; `--output-dir` overrides it.
Native tarballs contain the canonical runtime tree under `vendor/<target>/`;
the launcher and SDK do not embed native payloads.

When intentionally reusing GitHub artifacts instead, replace `--vendor-src` with
an explicit `--workflow-url`, or supply `--workflow-name` and, where needed,
`--github-repo`. There is no implicit workflow name and no fallback to an upstream
workflow. Local vendor input cannot be combined with workflow artifact options.
That route downloads artifacts and requires the configured GitHub tooling/access.

`codex-cli/scripts/build_npm_package.py` remains the package-specific entry point.
It accepts `--package`, `--version` (or `--release-version`), `--staging-dir`, and
`--pack-output`; native packages additionally require `--vendor-src` pointing at
a prehydrated canonical vendor tree. Use the root staging helper for a matching
set of CLI, native, and SDK tarballs, then follow the
[local install instructions](../README.npm.md).
