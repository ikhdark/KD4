# Codex CLI

This is the npm launcher for the local, Windows-only KD4 checkout of OpenAI Codex.
It is not an installation guide for the upstream npm distribution. See the
[repository README](../README.md) for the local build and setup workflow.

## Install locally staged tarballs

Use the CLI and native tarballs staged from the same local package inputs.
The launcher requires Node.js 16+. From the directory containing the tarballs,
on Windows x64:

```powershell
$version = "0.0.0-dev" # replace with the version used while staging
npm install -g ".\codex-npm-$version.tgz" "@openai/codex-win32-x64@file:.\codex-npm-win32-x64-$version.tgz"
```

On Windows ARM64, use
`@openai/codex-win32-arm64@file:.\codex-npm-win32-arm64-$version.tgz` instead.
The main tarball contains only the launcher; the native tarball contains the
canonical runtime tree. Installing only the main tarball does not supply that
runtime. The launcher's alias pin requires the exact KD4 native version, so do
not mix it with an upstream registry package.

Run `codex` after local installation. Use your configured Codex authentication;
[upstream authentication documentation](https://developers.openai.com/codex/auth)
describes the account flows, not publication or deployment of this checkout.

[Local npm staging](scripts/README.md) creates tarballs without publishing them.
Publishing or republishing requires explicit permission.

This project is licensed under the Apache-2.0 License.
