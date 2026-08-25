# Codex Package Scripts

Agent workflow rules for this directory live in `AGENTS.md`. This README
describes the package builder used by release workflows, local packaging, npm
staging, installers, and the Python runtime artifact flow.

## Entrypoint

Run the canonical wrapper from the repository root:

```sh
python scripts/build_codex_package.py \
  --target x86_64-pc-windows-msvc \
  --cargo-profile release \
  --package-dir dist/codex-package \
  --archive-output dist/codex-package-x86_64-pc-windows-msvc.zip
```

The wrapper routes to `codex_package.cli.main` and is the canonical packaging
entrypoint in this fork. Upstream release automation may wrap it to supply signed
or prebuilt artifacts and create additional archive formats, but those `.github`
workflow scripts are not present in this checkout.

## Package Layout

The layout version remains `1`:

```text
.
├── codex-package.json
├── bin
│   ├── <entrypoint>.exe
│   └── codex-code-mode-host.exe
├── codex-resources
│   ├── codex-command-runner.exe
│   └── codex-windows-sandbox-setup.exe
└── codex-path
    ├── rg.exe
    ├── apply_patch.bat
    └── applypatch.bat
```

`codex` and `codex-app-server` are supported entrypoint variants. The
`codex-code-mode-host` executable is always placed beside the selected
entrypoint because the runtime discovers it as a sibling process.

## Inputs And Source Builds

Without overrides, Cargo builds the entrypoint, code-mode host, and required
platform helpers in one package target lane. Release callers should pass the
already signed artifacts with `--entrypoint-bin`, `--code-mode-host-bin`, and
the applicable helper flags.

`--reuse-source-builds` reuses outputs only when the target/profile/variant,
source-tree fingerprint, and output fingerprints match. `--force-source-rebuild`
bypasses that reuse. `--skip-build-if-present` is a separate mode that requires
all expected package-lane outputs and cannot be combined with source overrides
or source-build reuse flags.

The CLI validates package/archive destinations,
duplicate outputs, and compression compatibility before starting Cargo builds
or downloads. `--reuse-package-dir` removes only package-managed paths and
preserves unrelated local contents; those unmanaged paths are excluded from
canonical archives.

## DotSlash Resources

Ripgrep comes from `rg` unless `--rg-bin` is supplied.

Downloaded archives and extracted executables are cached under the system temp
directory in `codex-package/`. Cache entries are verified against manifest size
and SHA-256 metadata, extracted through temporary files, and replaced only after
successful validation.

## Archives And Validation

Supported outputs are `.tar.gz`, `.tgz`, `.tar.zst`, and `.zip`. Archive writes
use a same-directory temporary file and atomically replace the destination, so a
failed forced rebuild preserves the previous archive.

Run the focused suite with:

```sh
python -m unittest discover -s scripts/codex_package -p 'test_*.py'
```

The suite covers target metadata, source-build reuse, CLI preflight, layout,
archive behavior, DotSlash resources, and version discovery.
