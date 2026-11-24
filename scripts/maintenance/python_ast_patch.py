"""Emit a guarded native patch for explicitly selected Python AST expressions."""

from __future__ import annotations

import argparse
import ast
import hashlib
import json
from pathlib import Path
import sys


def unique(nodes, selector):
    matches = list(nodes)
    if len(matches) != 1:
        raise ValueError(f"{selector}: expected one match, found {len(matches)}")
    return matches[0]


def assignment(body, name, selector):
    return unique(
        (
            node.value
            for node in body
            if isinstance(node, ast.Assign)
            and len(node.targets) == 1
            and isinstance(node.targets[0], ast.Name)
            and node.targets[0].id == name
        ),
        selector,
    )


def select(module, selector, value):
    parts = selector.split(".")
    if parts[0] == "import" and len(parts) >= 2:
        name = ".".join(parts[1:])
        node = unique(
            (
                node
                for node in module.body
                if isinstance(node, ast.Import)
                and len(node.names) == 1
                and node.names[0].name == name
            ),
            selector,
        )
        if not isinstance(value, str) or "\n" in value or "\r" in value:
            raise ValueError("import replacement must be a single import statement")
        [replacement] = ast.parse(value).body
        if not isinstance(replacement, ast.Import) or len(replacement.names) != 1:
            raise ValueError("import replacement must name one module")
        return node, value
    if len(parts) == 2 and parts[0] == "assignment":
        node = assignment(module.body, parts[1], selector)
    elif len(parts) == 4 and parts[0] == "function" and parts[2] == "assignment":
        function = unique(
            (
                node
                for node in module.body
                if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef))
                and node.name == parts[1]
            ),
            selector,
        )
        expr = assignment(function.body, parts[3], selector)
        if not isinstance(expr, ast.BinOp):
            raise ValueError(f"{selector}: expected a binary expression")
        node = expr.right
    else:
        raise ValueError(f"unsupported selector: {selector}")
    if not isinstance(node, ast.Constant) or type(value) not in (
        int,
        bool,
        str,
        type(None),
    ):
        raise ValueError(
            f"{selector}: expected a literal and a JSON integer, boolean, string or null"
        )
    return node, repr(value)


def patch(path, expected, edits):
    if not path or path.strip() != path or any(ch in path for ch in "\r\n\0"):
        raise ValueError("expected a patch-safe explicit file path")
    if not isinstance(edits, dict) or not 1 <= len(edits) <= 64:
        raise ValueError("expected 1–64 edit selectors")
    with Path(path).open("rb") as source:
        raw = source.read(1024 * 1024 + 1)
    if len(raw) > 1024 * 1024:
        raise ValueError("source exceeds 1 MiB")
    digest = hashlib.sha256(raw).hexdigest()
    if digest != expected:
        raise ValueError("source hash changed; reread before composing a new patch")
    bom = 3 if raw.startswith(b"\xef\xbb\xbf") else 0
    module = ast.parse(raw[bom:].decode("utf-8"), filename=path)
    offsets = [bom]
    for line in raw[bom:].splitlines(keepends=True):
        offsets.append(offsets[-1] + len(line))
    spans = sorted(
        (
            offsets[node.lineno - 1] + node.col_offset,
            offsets[node.end_lineno - 1] + node.end_col_offset,
            value.encode(),
        )
        for node, value in (select(module, key, value) for key, value in edits.items())
    )
    result = raw
    boundary = len(raw)
    for start, end, value in reversed(spans):
        if not 0 <= start < end <= boundary:
            raise ValueError("overlapping AST edits")
        result = result[:start] + value + result[end:]
        boundary = start
    compile(result[bom:].decode("utf-8"), path, "exec")
    def chunk(before, after, count=1):
        old = json.dumps(before.decode("utf-8"), ensure_ascii=True)
        new = json.dumps(after.decode("utf-8"), ensure_ascii=True)
        return f"@@ codex-replace {count} sha256:{digest}\n-{old}\n+{new}\n"

    options = [chunk(raw, result)]
    changed = [(start, end, value) for start, end, value in spans
               if raw[start:end] != value]
    if changed:
        start, end = changed[0][0], changed[-1][1]
        if raw.count(raw[start:end]) == 1:
            options.append(chunk(raw[start:end], result[start:len(result) - (len(raw) - end)]))
        groups = {}
        for start, end, value in changed:
            groups.setdefault((raw[start:end], value), []).append(start)
        local = []
        for (before, after), starts in groups.items():
            cursor = 0
            for start in starts:
                if raw.find(before, cursor) != start:
                    break
                cursor = start + len(before)
            else:
                if raw.find(before, cursor) == -1:
                    local.append(chunk(before, after, len(starts)))
                    continue
            break
        else:
            options.append("".join(local))
    chunks = min(options, key=len)
    return f"*** Begin Patch\n*** Update File: {path}\n{chunks}*** End Patch\n"


def unique_keys(pairs):
    values = {}
    for key, value in pairs:
        if key in values:
            raise ValueError(f"duplicate selector: {key}")
        values[key] = value
    return values


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path")
    parser.add_argument("source_sha256")
    parser.add_argument(
        "edits", help="JSON mapping of explicit selectors to replacement values"
    )
    args = parser.parse_args()
    try:
        edits = json.loads(args.edits, object_pairs_hook=unique_keys)
        sys.stdout.buffer.write(patch(args.path, args.source_sha256, edits).encode())
    except (OSError, ValueError, SyntaxError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
