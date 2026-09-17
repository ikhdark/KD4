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
- the active permission policy and returned error indicate that an important command failed because of sandbox restrictions. A network error alone does not establish that escalation will help.

Obtain consent for destructive actions not covered by the user's request. Consent alone does not require sandbox escalation; escalate only if the approved action needs unavailable permissions.

For a sandbox-related failure, request permissions with `require_escalated` and `justification`. Retry the whole command only when safe to repeat; otherwise resume the failed portion with its required inputs and working directory. Use the returned result to avoid replaying completed effects; inspect state only when uncertainty about those effects matters. Do not evade the approval flow by switching tools or techniques, or by delegating to another agent.

## Command segmentation

Segmentation follows the active shell. Simple pipelines and command lists can be evaluated independently. PowerShell `&` invokes a command, `-and`/`-or` are expression operators, and cmd.exe `^` escapes characters; do not interpret them as POSIX separators. Unsupported syntax may require approval for the whole invocation.

Commands using redirection, substitutions, environment assignments, or wildcard patterns are not matched against reusable rules because those features can broaden what a rule authorizes.

## Prefix rules

Choose the narrowest categorical prefix that covers similar intended commands. Simple PowerShell `-NoProfile` commands can match the invoked command's prefix. Never propose a bare interpreter or a script switch such as `["pwsh", "-Command"]`, `["cmd", "/c"]`, or `["python", "-"]`; these authorize arbitrary code.

Never provide `prefix_rule` for destructive commands, heredocs, or herestrings. Usually do not pass the entire command as the prefix.

Good examples:

- `["npm", "run", "dev"]`
- `["gh", "pr", "check"]`
- `["cargo", "test"]`
