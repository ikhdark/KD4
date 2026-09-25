# Codex CLI

Codex CLI is a coding agent from OpenAI that runs locally on Windows.

## Install

Install the staged KD4 tarball together with the native tarball for your
architecture from the same fork release. On Windows x64:

```shell
npm install -g ./codex-npm-${VERSION}.tgz "@openai/codex-win32-x64@file:./codex-npm-win32-x64-${VERSION}.tgz"
```

On Windows ARM64, use `@openai/codex-win32-arm64@file:./codex-npm-win32-arm64-${VERSION}.tgz`
instead. The native package is not published to the npm registry, so installing
the main tarball alone leaves Codex without its native binary.

Then run:

```shell
codex
```

On first launch, sign in with ChatGPT or configure an API key. See the
[Codex documentation](https://developers.openai.com/codex) for authentication,
configuration, sandboxing, and command-line usage.

## Windows standalone installer

`powershell -ExecutionPolicy ByPass -c "irm
https://raw.githubusercontent.com/ikhdark/KD4/main/scripts/install/install.ps1 | iex"`

Release archives are available from the
[KD4 GitHub releases](https://github.com/ikhdark/KD4/releases/latest).

This project is licensed under the Apache-2.0 License.
