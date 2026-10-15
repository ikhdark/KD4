#!/usr/bin/env python3
"""Query helper for a rolloutdetect `parse --json` analysis export.

Read-only. Lets many reviewers interrogate one large analysis JSON by dotted
path without each loading the whole document into a model context.

Usage (run with -I from the repository root):

  python -I -B scripts/benchmark_scripts/rdq.py <analysis.json> keys <path>
  python -I -B scripts/benchmark_scripts/rdq.py <analysis.json> get <path> [--depth N]
  python -I -B scripts/benchmark_scripts/rdq.py <analysis.json> find <regex> [--limit N]
  python -I -B scripts/benchmark_scripts/rdq.py <analysis.json> nulls <path>

`get` prints the value at a dotted path, truncating nested structure beyond
--depth so a single query cannot dump an unbounded subtree. `find` searches key
names anywhere in the document and reports each match's path and value kind.
`nulls` lists which immediate children of a path are null/zero/empty, which is
the usual question when checking a claim that some metric is uninstrumented.

Paths use dots for object keys and integers for array indexes, e.g.
`perTurn.0.usage` or `toolRelay.truncationEvents`.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from typing import Any

MAX_INLINE = 4000


def load(path: str) -> Any:
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


def resolve(doc: Any, path: str) -> Any:
    if not path or path in {".", "$"}:
        return doc
    node = doc
    for raw in path.split("."):
        if isinstance(node, list):
            try:
                node = node[int(raw)]
            except (ValueError, IndexError):
                raise KeyError(f"no index {raw!r} in array of {len(node)}")
        elif isinstance(node, dict):
            if raw not in node:
                raise KeyError(f"no key {raw!r}; have: {sorted(node)[:40]}")
            node = node[raw]
        else:
            raise KeyError(f"cannot descend into {type(node).__name__} at {raw!r}")
    return node


def kind(value: Any) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return f"bool({str(value).lower()})"
    if isinstance(value, (int, float)):
        return f"{type(value).__name__}({value})"
    if isinstance(value, str):
        text = value if len(value) <= 80 else value[:77] + "..."
        return f"str[{len(value)}]({text!r})"
    if isinstance(value, list):
        return f"array[{len(value)}]"
    return f"object[{len(value)}]"


def prune(value: Any, depth: int) -> Any:
    """Replace structure deeper than `depth` with a kind summary."""
    if depth <= 0:
        if isinstance(value, (dict, list)):
            return f"<{kind(value)}>"
        return value
    if isinstance(value, dict):
        return {k: prune(v, depth - 1) for k, v in value.items()}
    if isinstance(value, list):
        head = [prune(v, depth - 1) for v in value[:50]]
        if len(value) > 50:
            head.append(f"<+{len(value) - 50} more>")
        return head
    return value


def cmd_keys(doc: Any, args: argparse.Namespace) -> int:
    node = resolve(doc, args.path)
    if isinstance(node, dict):
        for key in node:
            print(f"{key:<48} {kind(node[key])}")
    elif isinstance(node, list):
        print(f"array[{len(node)}]")
        for index, item in enumerate(node[:50]):
            print(f"{index:<48} {kind(item)}")
    else:
        print(kind(node))
    return 0


def cmd_get(doc: Any, args: argparse.Namespace) -> int:
    node = resolve(doc, args.path)
    text = json.dumps(prune(node, args.depth), indent=2, default=str)
    if len(text) > MAX_INLINE and not args.full:
        print(text[:MAX_INLINE])
        print(
            f"\n<truncated at {MAX_INLINE} chars; narrow the path, lower --depth,"
            " or pass --full>"
        )
    else:
        print(text)
    return 0


def cmd_find(doc: Any, args: argparse.Namespace) -> int:
    pattern = re.compile(args.regex, re.IGNORECASE)
    hits = 0

    def walk(node: Any, path: str) -> None:
        nonlocal hits
        if hits >= args.limit:
            return
        if isinstance(node, dict):
            for key, value in node.items():
                here = f"{path}.{key}" if path else key
                if pattern.search(key):
                    print(f"{here:<70} {kind(value)}")
                    hits += 1
                    if hits >= args.limit:
                        return
                walk(value, here)
        elif isinstance(node, list):
            for index, value in enumerate(node):
                walk(value, f"{path}.{index}")

    walk(doc, "")
    if hits == 0:
        print(f"no key matching /{args.regex}/")
    elif hits >= args.limit:
        print(f"\n<stopped at --limit {args.limit}; more may exist>")
    return 0


def cmd_nulls(doc: Any, args: argparse.Namespace) -> int:
    node = resolve(doc, args.path)
    if not isinstance(node, dict):
        print(f"not an object: {kind(node)}")
        return 1
    empty: list[str] = []
    present: list[str] = []
    for key, value in node.items():
        if value is None or value == 0 or value == [] or value == {} or value == "":
            empty.append(f"{key:<48} {kind(value)}")
        else:
            present.append(f"{key:<48} {kind(value)}")
    print(f"== null/zero/empty ({len(empty)}) ==")
    print("\n".join(empty) if empty else "(none)")
    print(f"\n== populated ({len(present)}) ==")
    print("\n".join(present) if present else "(none)")
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("analysis", help="path to a rolloutdetect parse --json export")
    sub = parser.add_subparsers(dest="command", required=True)

    p_keys = sub.add_parser("keys", help="list immediate children of a path")
    p_keys.add_argument("path", nargs="?", default="")
    p_keys.set_defaults(func=cmd_keys)

    p_get = sub.add_parser("get", help="print the value at a path")
    p_get.add_argument("path", nargs="?", default="")
    p_get.add_argument("--depth", type=int, default=2)
    p_get.add_argument("--full", action="store_true")
    p_get.set_defaults(func=cmd_get)

    p_find = sub.add_parser("find", help="search key names by regex")
    p_find.add_argument("regex")
    p_find.add_argument("--limit", type=int, default=60)
    p_find.set_defaults(func=cmd_find)

    p_nulls = sub.add_parser("nulls", help="split children into empty vs populated")
    p_nulls.add_argument("path", nargs="?", default="")
    p_nulls.set_defaults(func=cmd_nulls)

    args = parser.parse_args(argv)
    try:
        doc = load(args.analysis)
    except OSError as exc:
        print(f"cannot read analysis: {exc}", file=sys.stderr)
        return 2
    try:
        return args.func(doc, args)
    except KeyError as exc:
        print(f"path error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
