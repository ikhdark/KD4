# argument-comment-lint

Isolated [Dylint](https://github.com/trailofbits/dylint) library for enforcing
Rust argument comments in the exact `/*param*/` shape.

Prefer self-documenting APIs over comment-heavy call sites when possible. If a
call site would otherwise read like `foo(false)` or `bar(None)`, consider an
enum, named helper, newtype, or another idiomatic Rust API shape first, and
use an argument comment only when a smaller compatibility-preserving change is
more appropriate.

It provides two lints:

- `argument_comment_mismatch` (`warn` by default): validates that a present
  `/*param*/` comment matches the resolved callee parameter name.
- `uncommented_anonymous_literal_argument` (`allow` by default): flags
  anonymous literal-like arguments such as `None`, `true`, `false`, and numeric
  literals when they do not have a preceding `/*param*/` comment.

String and char literals are exempt because they are often already
self-descriptive at the callsite.

The sole non-self method argument is also exempt when the method name matches
the resolved parameter name. For example, `.enabled(false)` is already
self-descriptive when it resolves to `fn enabled(self, enabled: bool)`. An
explicit argument comment is still checked for a mismatch.

## Behavior

Given:

```rust
fn create_openai_url(base_url: Option<String>, retry_count: usize) -> String {
    let _ = (base_url, retry_count);
    String::new()
}
```

This is accepted:

```rust
create_openai_url(/*base_url*/ None, /*retry_count*/ 3);
```

This is warned on by `argument_comment_mismatch`:

```rust
create_openai_url(/*api_base*/ None, 3);
```

This is only warned on when `uncommented_anonymous_literal_argument` is enabled:

```rust
create_openai_url(None, 3);
```

## Development

Install the required tooling once:

```powershell
cargo install cargo-dylint dylint-link
rustup toolchain install nightly-2025-09-18 `
  --component llvm-tools-preview `
  --component rustc-dev `
  --component rust-src
```

Run the lint crate tests:

```powershell
Set-Location tools/argument-comment-lint
cargo test
```

`just argument-comment-lint` runs `run.py`, which builds this crate from source
with `cargo dylint --path tools/argument-comment-lint` before checking
`codex-rs`, so the enforced rules are always the checked-in ones.

Run the lint against `codex-rs` from the repo root:

```powershell
just argument-comment-lint
just argument-comment-lint -p codex-core
python tools/argument-comment-lint/run.py -p codex-core
```

If no package selection is provided, `just argument-comment-lint` checks the
whole Cargo workspace. The wrapper defaults the underlying Cargo invocation to
`--all-targets` unless you explicitly narrow the target set, so targeted runs
cover test-only call sites by default. It also passes `--ignore-rust-version`:
the pinned lint nightly (Rust 1.92) is older than the `rust-version` some
`codex-rs` dependencies declare, and the lint only needs the code to type-check.

Repo runs also promote `argument_comment_mismatch` and
`uncommented_anonymous_literal_argument` to errors by default. The wrapper does
that by setting `DYLINT_RUSTFLAGS`, and it leaves an explicit level for either
lint alone. It also defaults `CARGO_INCREMENTAL=0` unless you have
already set it, because the current nightly Dylint flow can otherwise hit a
rustc incremental compilation ICE locally. To override that behavior for an ad
hoc run:

```powershell
$env:DYLINT_RUSTFLAGS = "-A argument-comment-mismatch -A uncommented-anonymous-literal-argument"
$env:CARGO_INCREMENTAL = "1"
python tools/argument-comment-lint/run.py -p codex-core
```

To override an explicitly narrow target selection, or to be explicit in scripts:

```powershell
python tools/argument-comment-lint/run.py -p codex-core -- --all-targets
```
