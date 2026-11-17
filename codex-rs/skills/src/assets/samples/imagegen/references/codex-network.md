# Codex network approval notes

This file is for the fallback CLI mode only. Read it when the user explicitly asks to use `scripts/image_gen.py` / CLI / API / model controls, or after the user explicitly confirms that a transparent-output request should use the `gpt-image-1.5` true-transparency fallback path.

This guidance is intentionally isolated from `SKILL.md` because it can vary by environment and may become stale. Prefer the defaults in your environment when in doubt.

## Why am I asked to approve image generation calls?
The fallback CLI uses the OpenAI Image API, so it needs outbound network access. Commands already run with full network access; a prompt comes only from the active approval policy (for example `on-request` or `untrusted`).

## How do I reduce repeated approval prompts?
If you trust the repo and want fewer prompts, use a configuration or profile whose approval policy matches your risk tolerance, or approve a narrowly scoped command prefix when asked.

Example `~/.codex/config.toml` pattern:

```toml
approval_policy = "on-request"
```

If you want quieter automation, you can choose a less interactive approval policy, but do that intentionally and with care.

## Safety note
Reducing approvals lowers friction, but increases risk if you run untrusted code or work in an untrusted repository.
