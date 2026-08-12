# Source tools

`locate_task`, `search_source`, and `read_file_span` are tools for grounded
source inspection. They are enabled by default. To disable them in
`config.toml`:

```toml
[features]
source_tools = false
```

Restart Codex after changing the feature setting.

## Boundaries

- The tools are available only for local environments. When a task has multiple
  environments, calls must select a local `environment_id`; remote environments
  are rejected.
- Repository paths are resolved against the canonical repository root. Symlinks
  and junctions cannot be used to escape that root.
- `read_file_span` may also read an exact installed `SKILL.md` path that was
  loaded for the current task. Other paths outside the repository are rejected.
- Results use repo-relative, one-based line-span citations.

## Task location

Call `locate_task` once when the owner or primary implementation is unknown. It
routes through the stable declarations in `source_owners.toml`, then lazily
indexes only the selected closure with embedded parser grammars. The result
combines source neighborhoods, conservative relationships, contracts, focused
tests, validation commands, and the applicable `AGENTS.md` chain with a
deterministic snapshot identifier. Unsupported semantic relationships remain
explicitly unresolved.

The disposable compact cache is stored below the active `CODEX_HOME`, partitioned
by canonical environment root and repository identity. Source bodies are not
persisted. A query admits at most 2,000 files and 16 MiB of captured source, and
the complete rendered result is capped at 8 KiB.

Use an exact path or symbol anchor to resolve ambiguity. Reuse a successful result
for the same task and snapshot. If a needed exact span is absent, make one
`read_file_span` call; use a narrowed `search_source` or anchored `locate_task`
only when the result says the owner or symbol remains unresolved.

Validate the routing manifest and managed `SOURCEMAP.md` block without rewriting:

```shell
python scripts/source_owners.py check
```

Run the representative local benchmark with:

```shell
cargo run -p codex-file-search --example locate_task_benchmark -- \
  --repository-root .. --task "shared kd4 source index"
```

Optional `--baseline-discovery-calls` and `--baseline-context-bytes` values add
an observed pre-locator baseline without fabricating one in the benchmark.

Regenerate only the explicitly marked block after intentional manifest changes:

```shell
python scripts/source_owners.py generate
```

## Search behavior and limits

`search_source` performs fixed-string matching. It follows repository
`.gitignore`, `.git/info/exclude`, and configured global Git excludes. Generated
or build-looking paths, vendored dependencies, and lockfiles are excluded unless
their corresponding `include_*` option is set.

Searches are deliberately bounded:

- 1,024-byte query and at most 32 search roots;
- 100 matches by default and at most 500 requested matches;
- at most 5 context lines before and after a match;
- at most 2,000 files, 16 MiB scanned, and 2 MiB read from any one file;
- at most 10,000 directories, 50,000 entries, and 64 directory levels walked;
- at most 512 KiB of serialized result text, with individual lines capped at
  4 KiB.

When a bound is reached, the result reports truncation or a limit reason rather
than continuing an unbounded scan.

## File reads

`read_file_span` reads an exact file span. It returns 120 lines by default,
accepts at most 400 lines per call, and returns at most 512 KiB.
