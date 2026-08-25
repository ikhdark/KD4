# Codex CLI

[**Codex CLI Documentation**](https://developers.openai.com/codex/cli)

## KD4 build and release runbook

Run these commands from the repository root. The supported local rollout is
`just publish-local-codex-final`; it runs the release-tooling gate, builds or
content-validates the four-binary release bundle, atomically activates it under
`Desktop\LOCAL-KD\bin`, and keeps runtime state under `Desktop\LOCAL-KD`.

The authoritative distributable package entrypoint is:

```powershell
python scripts/build_codex_package.py --target x86_64-pc-windows-msvc `
  --cargo-profile release --release-version <VERSION> `
  --package-dir _build/package-x64 --release-dir _build/release
```

Repeat it with `--target aarch64-pc-windows-msvc` and a distinct package
directory for ARM64. The release directory contains installer-named archives,
the checksum manifest, and provenance. Run only the focused package tests with
`python -m unittest discover -s scripts/codex_package -p 'test_*.py'`.

`just prepare-codex-release <VERSION>` produces both targets. Publication is
only supported through `just publish-codex-release <VERSION>`, which requires
keyless Sigstore bundles, verifies them against the exact authorized identity
and OIDC issuer supplied in `CODEX_RELEASE_CERTIFICATE_IDENTITY` and
`CODEX_RELEASE_OIDC_ISSUER`, uploads the complete inventory with `gh`, and
compares the remote asset list. Fulcio/Rekor and those two exact identity values
are the release trust root; unsigned manual uploads are unsupported.

Stage npm packages from those canonical trees with
`python scripts/stage_npm_packages.py --vendor-src <directory> --version
<VERSION> ...`; the stager rejects source/version/inventory mismatches. The
standalone installer is `scripts/install/install.ps1`; it consumes tags named
`rust-v<VERSION>` and the exact assets emitted above. Generic Cargo output,
source-tree npm packing, and manually assembled four-binary directories are not
supported release inputs.
