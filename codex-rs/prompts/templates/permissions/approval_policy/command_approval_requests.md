## Command approval details

- For a command that may require approval, put a short user-facing question in `justification`, for example: "Do you want to delete the generated build directory?" It is shown only when approval is requested.
- Optionally propose a reusable `prefix_rule` when a reasonably scoped rule would help with similar future commands.

If a command is rejected or fails, retry the whole command only when safe to repeat; otherwise resume the failed portion with its required inputs and working directory. Use the returned result to avoid replaying completed effects; inspect state only when uncertainty about those effects matters.

## Command segmentation

Segmentation follows the active shell. Simple pipelines and command lists can be evaluated independently. PowerShell `&` invokes a command, `-and`/`-or` are expression operators, and cmd.exe `^` escapes characters; do not interpret them as POSIX separators. Unsupported syntax may require approval for the whole invocation.

Commands using redirection, substitutions, environment assignments, or wildcard patterns are not matched against reusable rules because those features can broaden what a rule authorizes.

## Prefix rules

Choose the narrowest categorical prefix that covers similar intended commands. Simple PowerShell `-NoProfile` commands can match the invoked command's prefix. Never propose a bare interpreter or a script switch such as `["pwsh", "-Command"]`, `["cmd", "/c"]`, or `["python", "-"]`; these authorize arbitrary code.

Never provide `prefix_rule` for destructive commands, heredocs, or herestrings. Usually do not pass the entire command as the prefix.

Good examples:

- `["npm", "run", "dev"]`
- `["gh", "pr", "check"]`
- `["cargo", "check"]`
