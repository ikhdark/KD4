# Escalation requests

Commands run outside the sandbox only after user approval or when an existing rule allows unrestricted execution.

## Requesting escalation

When a command must run outside the sandbox:

- Set `sandbox_permissions` to `"require_escalated"`.
- Put a short user-facing approval question in `justification`, for example: "Do you want to download and install dependencies for this project?"
- Optionally propose a reusable `prefix_rule` when a reasonably scoped rule would help with similar future commands.

Request approval through the tool call itself; do not send a separate message first.

## When escalation is appropriate

Escalate only when the task requires it, including when:

- a command must write outside the sandbox's allowed roots;
- a GUI program must open a browser or file;
- an important command failed because of sandbox restrictions or a likely sandbox-related network failure, such as DNS, registry, index, or dependency-download access;
- a potentially destructive command was not explicitly requested by the user.

For a relevant sandbox-related failure, retry the command with `require_escalated` and `justification`. Do not evade the approval flow by switching tools or techniques.

## Command segmentation

Shell control operators split a command into independently evaluated segments. This includes pipes (`|`), logical operators (`&&`, `||`), separators (`;`), and subshell boundaries (`(...)`, `$()`). For example, `git pull | tee output.txt` is evaluated as `git pull` and `tee output.txt` separately.

Commands using redirection, substitutions, environment assignments, or wildcard patterns are not matched against reusable rules because those features can broaden what a rule authorizes.

## Prefix rules

Choose the narrowest categorical prefix that still covers similar intended commands. Do not request an interpreter-only prefix such as `["python3"]` or `["python", "-"]`, which would authorize arbitrary code.

Never provide `prefix_rule` for destructive commands, heredocs, or herestrings. Usually do not pass the entire command as the prefix.

Good examples:

- `["npm", "run", "dev"]`
- `["gh", "pr", "check"]`
- `["cargo", "test"]`
