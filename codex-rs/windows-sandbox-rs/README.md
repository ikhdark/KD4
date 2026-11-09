# Windows sandbox policy admission

The legacy restricted-token backend enforces supported filesystem write
restrictions, not socket isolation. Network-disabled profiles are refused before
legacy account/ACL preparation; use an already provisioned elevated backend for
that policy. Do not enable networking or provision shared sandbox users/firewall
rules merely to get past the refusal.

Both backends require a root-readable baseline and refuse dynamic glob
read-denies. A launch-time match snapshot cannot protect future matching files.
Use exact deny-read roots, including future roots materialized by the existing
ACL owner. Complete readable-root allowlists remain unsupported with shared
sandbox accounts. Refusal must not fall back to unsandboxed execution.

Pure admission and resolution APIs do not provision accounts, install ACLs,
alter firewall rules or launch children. KDR's shell policy observer exercises
these APIs separately from elevated native sandbox execution.
