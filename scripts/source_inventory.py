#!/usr/bin/env python3
"""Bounded, evidence-backed source inventory. Run --help for the query format.

Classification means a declared rule matched, not proof of runtime reachability.
Ambiguous rules must set unresolved=true until their consumers are inspected.
The state file belongs to the task: reuse it for subsequent queries.
"""

import argparse
import fnmatch
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import sys

if __package__:
    from .source_map_check import repository_source_records
else:
    from source_map_check import repository_source_records

PRUNE = (".git", "target", "node_modules", "build", "dist", ".next", ".venv", "__pycache__")
MAX_FILE_BYTES = 8 * 1024 * 1024
MAX_SCAN_BYTES = 64 * 1024 * 1024


def digest(value):
    return hashlib.sha256(value).hexdigest()


def normalized_path(value):
    path = PurePosixPath(value.replace("\\", "/"))
    if path.is_absolute() or ".." in path.parts or not path.parts or ":" in path.parts[0]:
        raise ValueError(f"expected a repository-relative candidate path: {value!r}")
    return path.as_posix()


def compile_categories(query):
    categories = {}
    for rule in query["categories"]:
        name = rule["name"]
        if name in categories or not name or not rule.get("paths"):
            raise ValueError("categories require unique names and nonempty paths")
        categories[name] = (rule, re.compile(rule["contains"]) if rule.get("contains") else None)
    if not categories:
        raise ValueError("at least one category is required")
    return categories


def inventory(root, query, previous=None):
    root = root.resolve()
    categories = compile_categories(query)
    states = repository_source_records(root, prune=PRUNE)
    previous = previous or {}
    old_files = previous.get("files", {}) if previous.get("root") == str(root) else {}
    records = {}
    files = {}
    read_bytes = 0
    searched = reused = 0
    coverage = {name: {"matched": 0, "unresolved": []} for name in categories}
    requested = {}
    for candidate in query.get("candidates", []):
        path = normalized_path(candidate["path"])
        name = candidate["category"]
        if name not in categories:
            raise ValueError(f"unknown category: {name}")
        requested.setdefault(path, set()).add(name)

    for path in sorted(set(states) | set(requested)):
        matching = {name for name, (rule, _) in categories.items()
                    if any(fnmatch.fnmatchcase(path, pattern) for pattern in rule["paths"])}
        selected = matching | requested.get(path, set())
        if not selected:
            continue
        state = states.get(path, "not_enumerated")
        full_path = root / path
        excluded = any(part in PRUNE for part in PurePosixPath(path).parts[:-1])
        # Do not follow symlinks outside the inventory boundary or probe an
        # explicitly excluded candidate inside a pruned tree.
        exists = None if excluded else full_path.is_file()
        error = None
        data = None
        content_hash = None
        if excluded:
            error = "excluded directory"
        elif state == "not_enumerated":
            error = "not in enumerated source set"
        elif not exists or state == "deleted":
            error = "deleted or missing"
        elif full_path.is_symlink() or not full_path.resolve().is_relative_to(root):
            error = "symlink source requires separate inspection"
        else:
            try:
                size = full_path.stat().st_size
                if size > MAX_FILE_BYTES or read_bytes + size > MAX_SCAN_BYTES:
                    error = "bounded read budget exceeded"
                else:
                    with full_path.open("rb") as source:
                        data = source.read(min(MAX_FILE_BYTES, MAX_SCAN_BYTES - read_bytes) + 1)
                    read_bytes += len(data)
                    if len(data) > MAX_FILE_BYTES or read_bytes > MAX_SCAN_BYTES:
                        data = None
                        error = "bounded read budget exceeded"
                    else:
                        content_hash = digest(data)
            except OSError as exc:
                error = str(exc)
        old = old_files.get(path, {})
        entries = {}
        text = None
        for name in sorted(selected):
            rule, pattern = categories[name]
            rule_hash = digest(json.dumps(rule, sort_keys=True).encode())
            prior = old.get("categories", {}).get(name, {})
            evidence = None
            reason = error
            matched = False
            if reason is None and name in matching:
                if old.get("sha256") == content_hash and prior.get("rule_hash") == rule_hash:
                    evidence = prior.get("evidence")
                    matched = prior.get("matched", False)
                    reason = prior.get("reason")
                    reused += 1
                else:
                    searched += 1
                    if pattern is None:
                        matched = True
                        evidence = {"path": path, "sha256": content_hash, "rule": name}
                    else:
                        try:
                            if text is None:
                                text = data.decode("utf-8")
                            match = pattern.search(text)
                            if match:
                                matched = True
                                evidence = {"path": path, "sha256": content_hash,
                                            "line": text.count("\n", 0, match.start()) + 1,
                                            "rule": name}
                        except UnicodeDecodeError:
                            reason = "non-UTF-8 source requires separate inspection"
                if not matched and reason is None:
                    reason = "category rule did not match"
            elif reason is None:
                reason = "candidate does not match category path rule"
            entries[name] = {"rule_hash": rule_hash, "matched": matched,
                             "evidence": evidence, "reason": reason}
            if not matched and path not in requested and reason == "category rule did not match":
                continue
            unresolved = reason or ("runtime consumer requires inspection" if rule.get("unresolved") else None)
            record = {"path": path, "category": name, "exists": exists,
                      "tracking": state, "evidence": evidence, "unresolved": unresolved}
            records[(path, name)] = record
            if unresolved:
                coverage[name]["unresolved"].append(path)
            else:
                coverage[name]["matched"] += 1
        if content_hash is not None:
            files[path] = {"sha256": content_hash, "categories": entries}

    result = list(records.values())
    validated = [record for record in result if record["unresolved"] is None]
    tracked = sorted({record["path"] for record in validated if record["tracking"] == "tracked"})
    untracked = sorted({record["path"] for record in validated if record["tracking"] == "untracked"})
    by_category = {name: sorted({r["path"] for r in validated
                                if r["category"] == name and r["tracking"] == "tracked"})
                   for name in categories}
    output = {"paths": tracked, "count": len(tracked), "untracked_paths": untracked,
              "untracked_count": len(untracked), "categories": by_category,
              "category_counts": {name: len(paths) for name, paths in by_category.items()},
              "unresolved": [r for r in result if r["unresolved"]],
              "coverage": coverage, "searched_records": searched, "reused_records": reused}
    state = {"version": 1, "root": str(root), "files": files, "records": result,
             "query": query, "output": output}
    return output, state


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, epilog='Query JSON: {"categories": [{"name": "templates", "paths": ["src/templates/*.md"], "contains": "optional regex", "unresolved": false}], "candidates": [{"path": "src/a.md", "category": "templates"}]}')
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--query", type=Path, required=True)
    parser.add_argument("--state", type=Path, required=True, help="Task-owned retained records and coverage; reused on subsequent calls")
    parser.add_argument("--paths", action="store_true", help="Emit the exact validated tracked path list for the final answer")
    args = parser.parse_args(argv)
    query = json.loads(args.query.read_text(encoding="utf-8"))
    previous = json.loads(args.state.read_text(encoding="utf-8")) if args.state.exists() else None
    output, state = inventory(args.root, query, previous)
    args.state.parent.mkdir(parents=True, exist_ok=True)
    args.state.write_text(json.dumps(state, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    if args.paths:
        print("\n".join(output["paths"]))
    else:
        # Retain full records; routine discovery receives only minimal data.
        summary = {key: output[key] for key in ("count", "untracked_count", "category_counts", "searched_records", "reused_records")}
        summary["unresolved_count"] = len(output["unresolved"])
        summary["artifact"] = str(args.state.resolve())
        print(json.dumps(summary, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, KeyError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
