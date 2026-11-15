# codex-utils-stream-parser

Small, dependency-free utilities for parsing streamed text incrementally.

The stateful [line parser](src/tagged_line_parser.rs) handles tag prefixes split
across chunks. Preserve its buffering and segment-ordering behavior when changing
the [plan parser](src/proposed_plan.rs); chunk boundaries are not line boundaries.

## What it provides

- `StreamTextParser`: trait for incremental parsers that consume string chunks
- `ProposedPlanParser`: splits plan-mode output into visible text and `<proposed_plan>` segments
- `AssistantTextStreamParser`: applies `ProposedPlanParser` in plan mode and passes text through
  unchanged otherwise
- `strip_proposed_plan_blocks(...)` / `extract_proposed_plan_text(...)`: one-shot helpers for
  non-streamed strings

## Why this exists

Plan-mode model output arrives as a stream, and a `<proposed_plan>` tag line can be split across
chunk boundaries (`<proposed` + `_plan>`). Parsing each chunk independently is incorrect.

This crate keeps parser state across chunks, returns visible text safe to render immediately, and
extracts plan segments separately.

Consume `finish()` at the end of each stream or item as well as every `push_str()`
result. It can release a buffered partial tag or emit the closing segment for an
unterminated plan block; an empty result from one push does not mean completion.

## Example: plan streaming

```rust
use codex_utils_stream_parser::ProposedPlanParser;
use codex_utils_stream_parser::ProposedPlanSegment;
use codex_utils_stream_parser::StreamTextParser;

let mut parser = ProposedPlanParser::new();

let first = parser.push_str("Intro\n<proposed");
assert_eq!(first.visible_text, "Intro\n");

let second = parser.push_str("_plan>\n- step\n</proposed_plan>\nOutro");
assert_eq!(second.visible_text, "Outro");
assert_eq!(
    second.extracted,
    vec![
        ProposedPlanSegment::ProposedPlanStart,
        ProposedPlanSegment::ProposedPlanDelta("- step\n".to_string()),
        ProposedPlanSegment::ProposedPlanEnd,
        ProposedPlanSegment::Normal("Outro".to_string()),
    ]
);

assert!(parser.finish().is_empty());
```

## Known limitations

- Tags are case-sensitive and must be alone on a line; surrounding whitespace is ignored
- No nested tag support
- A stream can return empty objects.
