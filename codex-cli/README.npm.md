# Codex CLI

Codex CLI is a coding agent from OpenAI that runs locally on Windows.

## Install

Install the staged KD4 tarball from the matching fork release. For example:

```shell
npm install -g ./codex-npm-${VERSION}.tgz
```

Then run:

```shell
codex
```

On first launch, sign in with ChatGPT or configure an API key. See the
[Codex documentation](https://developers.openai.com/codex) for authentication,
configuration, sandboxing, and command-line usage.

## Windows standalone installer

`powershell -ExecutionPolicy ByPass -c "irm https://raw.githubusercontent.com/ikhdark/KD4/main/scripts/install/install.ps1 | iex"`

Release archives are available from the
[KD4 GitHub releases](https://github.com/ikhdark/KD4/releases/latest).

This project is licensed under the Apache-2.0 License.
