---
name: "imagegen"
description: "Generate or edit raster images when the task benefits from AI-created bitmap visuals such as photos, illustrations, textures, sprites, mockups, or transparent-background cutouts. Use when Codex should create a brand-new image, transform an existing image, or derive visual variants from references, and the output should be a bitmap asset rather than repo-native code or vector. Do not use when the task is better handled by editing existing SVG/vector/code-native assets, extending an established icon or logo system, or building the visual directly in HTML/CSS/canvas."
---

# Image Generation Skill

Generate or edit raster assets; preserve existing SVG/vector/code-native systems when those are the requested output. Do not substitute vector placeholders for an explicitly raster request.

## Execution mode

| Request | Route |
|---|---|
| Normal generation/editing, including many assets or variants | Built-in `image_gen`; one call per asset or variant. No API key needed. |
| Simple transparent output | Built-in generation followed by local chroma-key removal; read [transparency guidance](references/prompting.md#transparent-images). |
| Explicit CLI/API/model workflow | Bundled `scripts/image_gen.py`; read [CLI usage](references/cli.md) and relevant [API controls](references/image-api.md). Requires `OPENAI_API_KEY`. |
| Native transparency, complex cutouts, failed chroma-key validation, or unavailable built-in tool | Explain the CLI fallback and obtain explicit consent unless that route was already requested. |

Ordinary quality, size, destination-path control, or the word "batch" does not select CLI mode. Never silently downgrade to CLI `gpt-image-1.5`: true native transparency uses `--background transparent --output-format png` with that model because `gpt-image-2` does not support `background=transparent`. Existing explicit requests for `gpt-image-1.5`, `scripts/image_gen.py`, or CLI fallback authorize that route.

Use the bundled CLI rather than one-off SDK runners. Do not modify `scripts/image_gen.py`; ask if a missing capability prevents the requested workflow.

## Workflow

1. Distinguish generation from editing. Images supplied only for style/composition are references. If an edit target is missing, locate it or request the attachment; do not turn an edit into new generation.
2. Gather exact text, constraints, input images, and intended use. Label each image's role and index. Inspect unseen local targets with `view_image`, then follow the live tool's reference-image contract. Preserve edit invariants.
3. Shape the prompt as scene, subject, relevant details, and constraints. Normalize detailed requests without inventing creative requirements. For generic requests add only useful framing, polish, layout, or scene detail; do not invent objects, brands, slogans, palettes, narrative beats, or arbitrary placement. Ask only for a critical missing detail.
4. Use concise labeled fields when helpful: use-case slug, asset type, primary request, input roles, scene, subject, style, framing, lighting, palette/materials, exact quoted text, and keep/avoid constraints. These are prompt scaffolding, not tool arguments. Follow the exact use-case slugs and task-specific guidance in [prompting.md](references/prompting.md); use [sample-prompts.md](references/sample-prompts.md) only for relevant recipes.
5. Generate using the selected route. In CLI batches, distinct assets require separate jobs; `n` produces variants of one prompt.
6. Inspect subject, style, composition, text accuracy, and invariants. Iterate with one targeted change, repeating edit invariants. For transparency, validate alpha and edges before use.
7. Save and integrate the selected deliverables as below. For a standalone image, let the displayed image be the response. For project work, continue integration/verification and report final paths; include prompts or fallback details only when requested or material.

## Save and integrate

Built-in outputs normally live under `$CODEX_HOME/generated_images/...`, not OS temp. Generate first, then copy the selected result; do not rely on a destination-path argument.

- Honor the user's destination; otherwise copy project assets into the workspace and update consuming code/references.
- Persist every requested final asset in a multi-asset task unless explicitly preview-only; discarded variants need not be retained.
- Preview-only files may remain at the built-in location and be displayed inline.
- Preserve generated originals unless moving/removing them was explicitly requested. Do not overwrite an existing asset without replacement authorization; use a versioned sibling name instead.

For chroma-key removal, use the installed helper at `$CODEX_HOME/skills/.system/imagegen/scripts/remove_chroma_key.py`, not a presumed project-relative script. Pillow is required; prefer `uv pip install pillow` in uv-managed environments or the active environment's package manager. If dependencies cannot be installed, explain the missing package and installation path.

## Conditional references

- [Prompting](references/prompting.md): use-case slugs, detailed prompt shaping, and the complete chroma-key procedure.
- [Sample prompts](references/sample-prompts.md): asset-type recipes and examples.
- [CLI](references/cli.md): load only for an authorized CLI route; covers setup, subcommands, batching, temp/output paths.
- [Image API](references/image-api.md): CLI model, quality, size, masks, output, and fidelity controls; do not assume these are built-in arguments.
- [Network troubleshooting](references/codex-network.md): CLI network/sandbox failures.

Never ask for an API key for built-in generation or ask the user to paste a secret into chat. For live CLI calls, have them set `OPENAI_API_KEY` locally; if missing, direct them to [API keys](https://platform.openai.com/api-keys) and explain environment-variable setup for their shell.
