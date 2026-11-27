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
When staged together, the SDK smoke install uses this run's launcher and
host-native tarballs, without resolving the CLI from a registry. The SDK's
other dependencies still use normal npm resolution; for an offline smoke
install, set `NPM_CONFIG_OFFLINE=true` and point `NPM_CONFIG_CACHE` at a prepared
cache containing those dependencies.
Staging `codex-sdk` also runs `pnpm install --frozen-lockfile` (including dependency
lifecycle scripts) and builds the SDK, even with local `--vendor-src`. It may
download dependencies; local staging is not an offline/no-install check. Inspect
the selected workload and obtain any required setup authorization first.
The helper writes tarballs to `dist/npm/` by default; `--output-dir` overrides it.
Native tarballs contain the canonical runtime tree under `vendor/<target>/`;
the launcher and SDK do not embed native payloads.
Current KD4 native packages do not require or build sandbox helpers.

When intentionally reusing GitHub artifacts instead, replace `--vendor-src` with
an explicit `--workflow-url`, or supply `--workflow-name` and, where needed,
`--github-repo`. There is no implicit workflow name and no fallback to an upstream
workflow. Native staging without a vendor source or workflow selector fails before
GitHub repository discovery. Local vendor input cannot be combined with workflow artifact options.
That route downloads artifacts and requires the configured GitHub tooling/access.

`codex-cli/scripts/build_npm_package.py` remains the package-specific entry point.
It accepts `--package`, `--version` (or `--release-version`), `--staging-dir`, and
`--pack-output`; native packages additionally require `--vendor-src` pointing at
a prehydrated canonical vendor tree. Use the root staging helper for a matching
set of CLI, native, and SDK tarballs, or pass both `--cli-tarball` and
`--native-tarball` when packing an SDK against an already staged local set.
An SDK pack with no local pair still resolves its dependencies normally.
`--pack-output` runs the smoke install before returning the tarball. For a
no-install diagnostic of the launcher or a prehydrated native package, stage
with `build_npm_package.py --staging-dir` **without** `--pack-output`, then run
`npm pack --ignore-scripts` in that staged directory. Do not use a startup
hook to suppress installation. This route does not qualify smoke behavior.
For CLI installation, follow the
[local install instructions](../README.npm.md).
