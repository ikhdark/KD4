# QA Checklist

Use for a broad or risky review. Remove checks that do not apply.

## Scope

<reviewed change, files, and relevant runtime paths>

## Checks

- [ ] The change satisfies the user's objective through the intended runtime path.
- [ ] Relevant callers, configuration, contracts, and generated outputs agree.
- [ ] Tests cover the changed behavior and important edge cases.
- [ ] Validation results support the claims; failures and skipped checks are explained.
- [ ] Desktop activation is verified if requested, or any required activation is reported.

## Findings and Evidence

| Finding                      | Source or check result                | Action             |
| ---------------------------- | ------------------------------------- | ------------------ |
| <issue, ordered by severity> | <file, command, or existing evidence> | <fix or follow-up> |

## Remaining Work

<unresolved findings or unverified behavior; write none when complete>
