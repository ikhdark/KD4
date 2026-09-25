#!/usr/bin/env python3
"""Bounded, evidence-backed source inventory. Run --describe for the public contract.

Categories default to runtime verification: matches remain unresolved until a
decision supplies exact consumer evidence. Use verification=path for inventories
that claim only file/rule matches. The state file belongs to the task; --render-only
renders that retained snapshot without scanning the repository again.
"""

import argparse
import fnmatch
import hashlib
import html
import json
import re
import subprocess
import sys
import uuid
from pathlib import Path, PurePosixPath

if __package__:
    from .atomic_json import write_bytes_atomic, write_json_atomic
else:
    from atomic_json import write_bytes_atomic, write_json_atomic

PRUNE = (
    ".git",
    "target",
    "node_modules",
    "build",
    "dist",
    ".next",
    ".venv",
    "__pycache__",
)
MAX_FILE_BYTES = 8 * 1024 * 1024
MAX_SCAN_BYTES = 64 * 1024 * 1024
RESULT_FORMAT = "source_inventory_result_v1"
PAGE_RECORDS = 50
SUMMARY_PAGE_BYTES = 16 * 1024


def repository_source_records(
    repo_root: Path, *, include_untracked: bool = True, prune: tuple[str, ...] = (),
    paths: tuple[str, ...] = (),
) -> dict[str, str]:
    # One listing answers everything the inventory needs: `--deleted` tags
    # tracked paths missing from the working tree (R) and `--stage` exposes
    # the index mode, so gitlinks (160000) drop out without a stat per path.
    args = [
        "git",
        "ls-files",
        "-t",
        "--stage",
        "--cached",
        "--deleted",
        "--exclude-standard",
        "-z",
    ]
    if include_untracked:
        args.append("--others")
    # Git prunes these untracked directories before descending. Filtering its
    # output alone would still walk arbitrarily large build/dependency trees.
    args.extend(f"--exclude={name}/" for name in prune)
    if paths:
        args.extend(["--", *(f":(literal){path}" for path in paths)])
    result = subprocess.run(
        args,
        cwd=repo_root,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if result.returncode != 0:
        detail = result.stderr.strip() or f"git ls-files exited {result.returncode}"
        raise ValueError(f"failed to enumerate repository sources: {detail}")
    deleted_paths: set[str] = set()
    entries: list[tuple[str, str]] = []
    for record in result.stdout.split("\0"):
        if not record:
            continue
        if len(record) < 3 or record[1] != " ":
            raise ValueError("git ls-files returned an invalid tagged path record")
        tag, body = record[0], record[2:]
        if tag == "?":
            path = PurePosixPath(body).as_posix()
        else:
            stage_fields, separator, path_text = body.partition("\t")
            if not separator:
                raise ValueError("git ls-files returned an invalid staged path record")
            mode = stage_fields.split(" ", 1)[0]
            path = PurePosixPath(path_text).as_posix()
            if tag == "R":
                deleted_paths.add(path)
                continue
            if mode == "160000":
                # A submodule gitlink is a directory in the working tree.
                continue
        entries.append((tag, path))
    records: dict[str, str] = {}
    for tag, path in entries:
        if any(part in prune for part in PurePosixPath(path).parts[:-1]):
            continue
        records[path] = "deleted" if path in deleted_paths else (
            "untracked" if tag == "?" else "tracked"
        )
    return records


def digest(value):
    return hashlib.sha256(value).hexdigest()


def query_profile(query):
    """Classification decisions do not change the discovery scope."""
    return {
        "categories": sorted(query["categories"], key=lambda rule: rule["name"]),
        "required_categories": sorted(
            set(
                query.get(
                    "required_categories", [r["name"] for r in query["categories"]]
                )
            )
        ),
        "candidates": sorted(
            query.get("candidates", []), key=lambda r: (r["category"], r["path"])
        ),
    }


def query_identity(query):
    return digest(
        json.dumps(query_profile(query), sort_keys=True, ensure_ascii=False).encode(
            "utf-8"
        )
    )


def json_shape(value, depth=0, budget=None):
    """Bounded structural evidence; string bodies never enter the display."""
    budget = [64] if budget is None else budget
    budget[0] -= 1
    if isinstance(value, str):
        return {"type": "string", "length": len(value)}
    if isinstance(value, (dict, list)):
        result = {
            "type": "object" if isinstance(value, dict) else "array",
            "length": len(value),
        }
        if depth >= 4 or budget[0] <= 0:
            result["omitted"] = len(value)
            return result
        items = (
            list(value.items())
            if isinstance(value, dict)
            else list(enumerate(value[:3]))
        )
        shown = {}
        for index, (key, child) in enumerate(items[:32]):
            if budget[0] <= 0:
                break
            label = str(key)
            if len(label) > 80:
                label = f"<field {index}: name length {len(label)}>"
            shown[label] = json_shape(child, depth + 1, budget)
        result["fields" if isinstance(value, dict) else "items"] = shown
        result["omitted"] = len(value) - len(shown)
        return result
    return {
        "type": "null"
        if value is None
        else "boolean"
        if isinstance(value, bool)
        else "number"
    }


def normalized_path(value):
    path = PurePosixPath(value.replace("\\", "/"))
    if (
        path.is_absolute()
        or ".." in path.parts
        or not path.parts
        or ":" in path.parts[0]
    ):
        raise ValueError(f"expected a repository-relative candidate path: {value!r}")
    return path.as_posix()


def discovery_paths(query):
    prefixes = set()
    for rule in query["categories"]:
        for pattern in rule["paths"]:
            normalized_path(pattern)
            fixed = re.split(r"[*?\[]", pattern, maxsplit=1)[0]
            prefix = (
                fixed
                if fixed == pattern
                else fixed.rsplit("/", 1)[0]
                if "/" in fixed
                else "."
            )
            prefixes.add(prefix.rstrip("/") or ".")
    prefixes.update(normalized_path(r["path"]) for r in query.get("candidates", []))
    return tuple(sorted(prefixes))


def instruction_paths(root, scopes):
    """Enumerate only selected subtrees and directly check their ancestors."""
    root = root.resolve()
    scopes = tuple(normalized_path(scope) for scope in scopes)
    if any(part in PRUNE for scope in scopes for part in PurePosixPath(scope).parts):
        raise ValueError("instruction scope is inside a pruned directory")
    records = repository_source_records(root, prune=PRUNE, paths=scopes)
    names = {"AGENTS.md", "AGENTS.override.md"}
    found = {
        path
        for path, status in records.items()
        if status != "deleted" and PurePosixPath(path).name in names
    }
    for scope in scopes:
        path = root / scope
        if not path.resolve().is_relative_to(root) or path.is_symlink():
            raise ValueError("instruction scope must remain inside the repository")
        directory = path if path.is_dir() else path.parent
        while directory.is_relative_to(root):
            for name in names:
                candidate = directory / name
                if candidate.is_file() and not candidate.is_symlink():
                    found.add(candidate.relative_to(root).as_posix())
            if directory == root:
                break
            directory = directory.parent
    return sorted(found)


def compile_categories(query):
    categories = {}
    for rule in query["categories"]:
        name = rule["name"]
        if name in categories or not name or not rule.get("paths"):
            raise ValueError("categories require unique names and nonempty paths")
        if rule.get("verification", "runtime") not in ("path", "runtime"):
            raise ValueError("verification must be path or runtime")
        categories[name] = (
            rule,
            re.compile(rule["contains"]) if rule.get("contains") else None,
        )
    if not categories:
        raise ValueError("at least one category is required")
    return categories


def consumer_evidence(root, references, cache):
    """Verify exact observations, not the reviewer's semantic interpretation."""
    if not references:
        return "runtime consumer requires inspection"
    for reference in references:
        path = normalized_path(reference["path"])
        full_path = root / path
        if path not in cache:
            if (
                any(part in PRUNE for part in PurePosixPath(path).parts[:-1])
                or full_path.is_symlink()
                or not full_path.resolve().is_relative_to(root)
            ):
                cache[path] = None
            else:
                try:
                    remaining = MAX_SCAN_BYTES - sum(
                        len(v) for v in cache.values() if v
                    )
                    limit = min(MAX_FILE_BYTES, remaining)
                    if full_path.stat().st_size > limit:
                        data = None
                    else:
                        with full_path.open("rb") as source:
                            data = source.read(limit + 1)
                        if len(data) > limit:
                            data = None
                    cache[path] = data
                except OSError:
                    cache[path] = None
        data = cache[path]
        if data is None or digest(data) != reference.get("sha256"):
            return "consumer evidence is unavailable or stale"
        try:
            lines = data.decode("utf-8").splitlines()
        except UnicodeDecodeError:
            return "consumer evidence is not UTF-8"
        line = reference.get("line")
        text = reference.get("text")
        if (
            type(line) is not int
            or not 1 <= line <= len(lines)
            or not isinstance(text, str)
            or not text.strip()
            or text != lines[line - 1]
        ):
            return "consumer evidence does not match the exact source line"
    return None


def inventory(root, query, previous=None, *, refresh=False):
    root = root.resolve()
    categories = compile_categories(query)
    previous = previous or {}
    if previous and previous.get("root") != str(root):
        raise ValueError("the retained state belongs to a different repository")
    query_id = query_identity(query)
    scope_changes = list(previous.get("scope_changes", []))
    if previous and query_identity(previous["query"]) != query_id:
        old_id = query_identity(previous["query"])
        change = query.get("scope_change", {})
        if (
            change.get("from_query_id") != old_id
            or not change.get("reason", "").strip()
        ):
            raise ValueError(
                f"query scope changed; retain the existing query or provide scope_change with from_query_id={old_id} and an explicit reason"
            )
        scope_changes.append(
            {
                "query_id": old_id,
                "profile": query_profile(previous["query"]),
                "reason": change["reason"],
                "count": previous["output"]["count"],
                "unresolved": previous["output"]["unresolved"],
                "excluded": previous["output"].get("excluded", []),
                "missing_categories": previous["output"].get("missing_categories", []),
            }
        )
    query = json.loads(json.dumps(query))
    if (
        "decisions" not in query
        and previous
        and query_identity(previous["query"]) == query_id
    ):
        query["decisions"] = previous["query"].get("decisions", [])
    decisions = {}
    for decision in query.get("decisions", []):
        key = (normalized_path(decision["path"]), decision["category"])
        if key in decisions or key[1] not in categories:
            raise ValueError("decisions require unique paths/categories from the query")
        if (
            decision.get("disposition") not in ("include", "exclude")
            or not decision.get("reason", "").strip()
        ):
            raise ValueError("decisions require include/exclude and a review reason")
        decisions[key] = decision
    evidence_cache = {}
    states = repository_source_records(root, prune=PRUNE, paths=discovery_paths(query))
    old_files = previous.get("files", {}) if previous.get("root") == str(root) else {}
    continuing = (
        not refresh
        and bool(previous.get("scan", {}).get("pending"))
        and query_identity(previous["query"]) == query_id
    )
    epoch = previous["scan"]["epoch"] if continuing else uuid.uuid4().hex
    pending = []
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
        matching = {
            name
            for name, (rule, _) in categories.items()
            if any(fnmatch.fnmatchcase(path, pattern) for pattern in rule["paths"])
        }
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
        retained = old_files.get(path) if continuing else None
        if excluded:
            error = "excluded directory"
        elif state == "not_enumerated":
            error = "not in enumerated source set"
        elif not exists or state == "deleted":
            error = "deleted or missing"
        elif full_path.is_symlink() or not full_path.resolve().is_relative_to(root):
            error = "symlink source requires separate inspection"
        elif retained is not None:
            # These are captured observations, not a claim about current bytes.
            # A refresh starts a new epoch and hashes every source again.
            content_hash = retained["sha256"]
        else:
            try:
                size = full_path.stat().st_size
                if size > MAX_FILE_BYTES:
                    error = "bounded read budget exceeded"
                elif read_bytes + size > MAX_SCAN_BYTES:
                    error = "scan batch budget exhausted; continue retained epoch"
                    pending.append(path)
                else:
                    with full_path.open("rb") as source:
                        data = source.read(
                            min(MAX_FILE_BYTES, MAX_SCAN_BYTES - read_bytes) + 1
                        )
                    read_bytes += len(data)
                    if len(data) > MAX_FILE_BYTES or read_bytes > MAX_SCAN_BYTES:
                        if len(data) <= MAX_FILE_BYTES:
                            pending.append(path)
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
                if (
                    old.get("sha256") == content_hash
                    and prior.get("rule_hash") == rule_hash
                ):
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
                                evidence = {
                                    "path": path,
                                    "sha256": content_hash,
                                    "line": text.count("\n", 0, match.start()) + 1,
                                    "rule": name,
                                }
                        except UnicodeDecodeError:
                            reason = "non-UTF-8 source requires separate inspection"
                    if matched and rule.get("json_summary"):
                        try:
                            evidence["structure"] = json_shape(json.loads(data))
                        except (ValueError, UnicodeDecodeError):
                            reason = "invalid JSON requires separate inspection"
                if not matched and reason is None:
                    reason = "category rule did not match"
            elif reason is None:
                reason = "candidate does not match category path rule"
            entries[name] = {
                "rule_hash": rule_hash,
                "matched": matched,
                "evidence": evidence,
                "reason": reason,
            }
            if (
                not matched
                and path not in requested
                and reason == "category rule did not match"
            ):
                continue
            runtime = rule.get("verification", "runtime") == "runtime" or rule.get(
                "unresolved", False
            )
            decision = decisions.get((path, name))
            unresolved = reason
            status = "matched"
            if unresolved is None and runtime:
                if decision is None:
                    unresolved = "runtime consumer requires inspection"
                elif decision.get("source_sha256") != content_hash:
                    unresolved = "classification source is stale"
                else:
                    unresolved = consumer_evidence(
                        root, decision.get("evidence"), evidence_cache
                    )
                    if unresolved is None:
                        status = (
                            "verified"
                            if decision["disposition"] == "include"
                            else "excluded"
                        )
            record = {
                "path": path,
                "category": name,
                "exists": exists,
                "tracking": state,
                "evidence": evidence,
                "unresolved": unresolved,
                "status": "unresolved" if unresolved else status,
                "decision": decision if runtime else None,
            }
            records[(path, name)] = record
            if unresolved:
                coverage[name]["unresolved"].append(path)
            else:
                coverage[name]["matched"] += 1
        if content_hash is not None:
            files[path] = {"sha256": content_hash, "categories": entries}

    result = list(records.values())
    validated = [
        record
        for record in result
        if record["unresolved"] is None and record["status"] != "excluded"
    ]
    tracked = sorted(
        {record["path"] for record in validated if record["tracking"] == "tracked"}
    )
    untracked = sorted(
        {record["path"] for record in validated if record["tracking"] == "untracked"}
    )
    by_category = {
        name: sorted(
            {
                r["path"]
                for r in validated
                if r["category"] == name and r["tracking"] == "tracked"
            }
        )
        for name in categories
    }
    output = {
        "query_id": query_id,
        "scope_changes": scope_changes,
        "scan_epoch": epoch,
        "scan_pending": len(pending),
        "source_bytes_read": read_bytes,
        "evidence_scope": "retained scan epoch; use --refresh to revalidate current workspace",
        "paths": tracked,
        "count": len(tracked),
        "untracked_paths": untracked,
        "untracked_count": len(untracked),
        "categories": by_category,
        "category_counts": {name: len(paths) for name, paths in by_category.items()},
        "unresolved": [r for r in result if r["unresolved"]],
        "coverage": coverage,
        "searched_records": searched,
        "reused_records": reused,
    }
    required = query.get("required_categories", list(categories))
    if not isinstance(required, list) or any(
        not isinstance(name, str) or not name for name in required
    ):
        raise ValueError("required_categories must be a list of category names")
    output["missing_categories"] = sorted(set(required) - set(categories))
    output["excluded"] = [r for r in result if r["status"] == "excluded"]
    output["ready_to_render"] = (
        not output["unresolved"] and not output["missing_categories"]
    )
    output["next_action"] = (
        "render" if output["ready_to_render"] else "resolve_remaining"
    )
    state = {
        "version": 3,
        "root": str(root),
        "files": files,
        "records": result,
        "scan": {"epoch": epoch, "pending": pending},
        "scope_changes": scope_changes,
        "query": query,
        "output": output,
    }
    return output, state


def render_report(state):
    """All identifiers and counts come from one retained snapshot."""
    output = state["output"]

    def code(text):
        return "<code>" + html.escape(str(text)) + "</code>"

    statuses = {(r["category"], r["path"]): r["status"] for r in state["records"]}
    lines = [
        "# Source inventory",
        "",
        "Retained snapshot; rendering does not refresh workspace evidence.",
        "",
        f"Root: {code(state['root'])}",
        "",
        f"Query: {code(output.get('query_id', query_identity(state['query'])))}",
        "",
        (
            f"Included tracked files: **{output['count']}**. "
            f"Untracked files: **{output['untracked_count']}**."
        ),
        "",
        (
            "Coverage is limited to the declared query. Consumer evidence records a "
            "reviewed classification; a path match alone does not establish runtime use."
        ),
        "",
        "Status: "
        + ("ready for the declared scope" if output["ready_to_render"] else "partial")
        + ".",
        "",
    ]
    for change in state.get("scope_changes", []):
        lines.extend(
            [
                "## Earlier scope (not covered by current completion)",
                "",
                f"Query: {code(change['query_id'])}. Change: {html.escape(change['reason'])}",
                "",
                f"Earlier required categories: {code(', '.join(change['profile']['required_categories']))}.",
                "",
                f"Earlier unresolved records: {len(change['unresolved'])}; reviewed exclusions: {len(change['excluded'])}.",
                "",
            ]
        )
        lines.extend(
            f"- Not examined: {code(name)}" for name in change["missing_categories"]
        )
        lines.extend(
            f"- Unresolved: {code(r['category'])}: {code(r['path'])}"
            for r in change["unresolved"]
        )
        lines.extend(
            f"- Excluded: {code(r['category'])}: {code(r['path'])}"
            for r in change["excluded"]
        )
        lines.append("")
    for name, paths in sorted(output["categories"].items()):
        lines.extend([f"## {code(name)} ({len(paths)})", ""])
        for path in paths:
            status = statuses[(name, path)]
            lines.append(f"- {code(path)} ({status})")
        if not paths:
            lines.append("No included tracked matches in the declared query.")
        lines.append("")
    if output["untracked_paths"]:
        lines.extend(
            ["## Untracked matches", ""]
            + [f"- {code(p)}" for p in output["untracked_paths"]]
            + [""]
        )
    lines.extend(["## Remaining work", ""])
    lines.extend(
        f"- Missing required category: {code(name)}"
        for name in output["missing_categories"]
    )
    lines.extend(
        f"- {code(r['category'])}: {code(r['path'])} — {html.escape(r['unresolved'])}"
        for r in output["unresolved"]
    )
    if output["ready_to_render"]:
        lines.append(
            "None for the declared scope. Reuse this report unless relevant inputs change."
        )
    if output["excluded"]:
        lines.extend(["", "## Reviewed exclusions", ""])
        lines.extend(
            f"- {code(r['path'])} — {html.escape(r['decision']['reason'])}"
            for r in output["excluded"]
        )
    return "\n".join(lines) + "\n"


def successful_json_summaries(state):
    """Public structural evidence, without internal cache layout or prompt bodies."""
    return [
        {
            "path": record["path"],
            "category": record["category"],
            "status": record["status"],
            "tracking": record["tracking"],
            "sha256": record["evidence"]["sha256"],
            "structure": record["evidence"]["structure"],
        }
        for record in sorted(
            state["records"], key=lambda record: (record["path"], record["category"])
        )
        if record["unresolved"] is None
        and record["status"] != "excluded"
        and record.get("evidence")
        and "structure" in record["evidence"]
    ]


def json_summary_page(summaries, offset):
    page = []
    size = 2
    for summary in summaries[offset : offset + PAGE_RECORDS]:
        encoded = json.dumps(summary, ensure_ascii=False).encode("utf-8")
        if size + len(encoded) + 2 > SUMMARY_PAGE_BYTES:
            if page:
                break
            # Keep progress possible even with unusually large retained metadata.
            # Full structural evidence remains in the public canonical delivery.
            summary = {
                **summary,
                "structure": {
                    "type": summary["structure"]["type"],
                    "detail_omitted": "Re-render retained state with --report PATH, then read json_summaries in canonical_paths for this record.",
                },
            }
            encoded = json.dumps(summary, ensure_ascii=False).encode("utf-8")
        page.append(summary)
        size += len(encoded) + 2
    next_offset = offset + len(page)
    return page, next_offset if next_offset < len(summaries) else None


def describe_contract():
    """Describe the supported interface without reading repository or state files."""
    return {
        "format": RESULT_FORMAT,
        "query": {
            "required_categories": "Names whose coverage must be retained; defaults to all declared categories.",
            "categories": [
                {
                    "name": "unique category name",
                    "paths": ["repository-relative globs"],
                    "verification": "path | runtime (default runtime)",
                    "contains": "optional Python regular expression",
                    "json_summary": "optional boolean; expose fields, types and lengths, never string bodies",
                    "unresolved": "legacy boolean; true requires runtime verification, false does not waive it",
                }
            ],
            "candidates": [
                {"path": "repository-relative path", "category": "declared category"}
            ],
            "decisions": [
                {
                    "path": "repository-relative path",
                    "category": "declared category",
                    "source_sha256": "observed source hash",
                    "disposition": "include | exclude",
                    "reason": "review reason",
                    "evidence": [
                        {
                            "path": "consumer path",
                            "sha256": "observed consumer hash",
                            "line": "1-based integer",
                            "text": "exact nonempty consumer source line",
                        }
                    ],
                }
            ],
            "scope_change": {
                "from_query_id": "previous query_id",
                "reason": "required when discovery scope changes",
            },
        },
        "example": {
            "required_categories": ["templates", "catalog"],
            "categories": [
                {
                    "name": "templates",
                    "paths": ["src/templates/*.md"],
                    "verification": "path",
                },
                {
                    "name": "catalog",
                    "paths": ["src/models.json"],
                    "verification": "path",
                    "json_summary": True,
                },
            ],
        },
        "result": {
            "format": RESULT_FORMAT,
            "query_id": "retained scope identity",
            "artifact": "state path",
            "count": "unique included tracked paths",
            "untracked_count": "unique included untracked paths",
            "category_counts": "tracked counts keyed by category; categories can overlap",
            "searched_records": "rules evaluated this scan",
            "reused_records": "unchanged rule results reused",
            "unresolved_count": "records requiring inspection",
            "missing_categories": "required undeclared categories",
            "earlier_scope_count": "retained scope changes",
            "ready_to_render": "true only for the declared scope",
            "next_action": "resolve_remaining | render | deliver_report",
            "paths": "with --paths: page of included tracked paths, never a replacement for this envelope",
            "paths_next_offset": "next path offset or null",
            "remaining": "with --remaining: page of path, category, unresolved reason and bounded evidence",
            "next_offset": "next unresolved offset or null",
            "json_summaries": [
                {
                    "path": "source path",
                    "category": "category",
                    "status": "matched | verified",
                    "tracking": "tracked | untracked",
                    "sha256": "source hash",
                    "structure": "bounded tree: type, length, fields/items, omitted; strings have lengths only",
                }
            ],
            "json_summary_count": "successful JSON records; present when nonzero",
            "json_summary_next_offset": "next summary offset or null",
            "report": "with --report: immutable Markdown path",
            "canonical_paths": "immutable JSON path",
            "delivery_sha256": "hash of canonical JSON bytes",
        },
        "paging": f"Use --state STATE --render-only --offset N without rescanning. Paths and remaining pages hold {PAGE_RECORDS} records; JSON summaries also have a {SUMMARY_PAGE_BYTES}-byte target. Each next-offset field advances its own list.",
        "delivery": "Canonical JSON contains all paths, categories, json_summaries, unresolved/excluded records and prior scope. Prefer its link to rereading or reprinting the report. A ready result proves only the declared query, not that its scope answers the entire task.",
    }


def export_delivery(state, report):
    """Content-addressed delivery survives later state/report revisions."""
    output = state["output"]
    document = {
        "format": RESULT_FORMAT,
        "query_id": output.get("query_id", query_identity(state["query"])),
        "profile": query_profile(state["query"]),
        "root": state["root"],
        "selection": "all validated tracked identifiers",
        "scan_pending": len(state.get("scan", {}).get("pending", [])),
        "evidence_scope": output.get("evidence_scope", "retained snapshot"),
        **{
            key: output[key]
            for key in (
                "paths",
                "count",
                "untracked_paths",
                "untracked_count",
                "categories",
                "unresolved",
                "excluded",
                "missing_categories",
                "ready_to_render",
            )
        },
        "scope_changes": state.get("scope_changes", []),
        "json_summaries": successful_json_summaries(state),
    }
    data = (
        json.dumps(document, ensure_ascii=False, sort_keys=True, indent=2) + "\n"
    ).encode("utf-8")
    identity = digest(data)
    canonical = report.with_name(f"{report.stem}-{identity}.json")
    readable = canonical.with_suffix(".md")
    rendered = (
        render_report(state)
        + f"\nCanonical path data: [{canonical.name}]({canonical.name})\n"
    ).encode("utf-8")
    for path, content in ((canonical, data), (readable, rendered)):
        write_bytes_atomic(path, content, immutable=True)
    write_bytes_atomic(report, rendered)
    return {
        "report": str(readable.resolve()),
        "canonical_paths": str(canonical.resolve()),
        "delivery_sha256": identity,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(
        description=__doc__,
        epilog="Use --describe for query fields, a path/JSON example, result fields, and paging. Successful JSON summaries are returned automatically when a category requests json_summary=true.",
    )
    parser.add_argument(
        "--describe",
        action="store_true",
        help="Print the versioned query/result contract without reading the repository",
    )
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--query", type=Path)
    parser.add_argument(
        "--state",
        type=Path,
        help="Task-owned retained records and coverage; reused on subsequent calls",
    )
    parser.add_argument(
        "--instructions",
        nargs="+",
        metavar="SCOPE",
        help="List applicable ancestor and scoped descendant instruction paths, pruning build/dependency trees before descent",
    )
    parser.add_argument(
        "--paths",
        action="store_true",
        help="Include a bounded page of exact tracked paths in the JSON result",
    )
    parser.add_argument(
        "--report",
        type=Path,
        help="Write a readable Markdown report from the retained records",
    )
    parser.add_argument(
        "--render-only",
        action="store_true",
        help="Read the state snapshot without rescanning or rewriting it",
    )
    parser.add_argument(
        "--refresh",
        action="store_true",
        help="Start a new scan epoch instead of continuing pending captured evidence",
    )
    parser.add_argument(
        "--remaining",
        action="store_true",
        help="Include a bounded page of unresolved retained records and structural evidence",
    )
    parser.add_argument(
        "--offset",
        type=int,
        default=0,
        help="Offset into selected path, unresolved, and JSON-summary pages",
    )
    args = parser.parse_args(argv)
    if args.describe:
        if (
            args.state
            or args.query
            or args.report
            or args.instructions
            or args.paths
            or args.render_only
            or args.remaining
            or args.offset
        ):
            parser.error("--describe is a standalone contract operation")
        print(json.dumps(describe_contract(), ensure_ascii=False))
        return 0
    if args.instructions:
        if (
            args.state
            or args.query
            or args.report
            or args.paths
            or args.render_only
            or args.remaining
        ):
            parser.error("--instructions is a standalone scoped discovery operation")
        print(
            json.dumps(
                {
                    "scopes": args.instructions,
                    "paths": instruction_paths(args.root, args.instructions),
                }
            )
        )
        return 0
    if args.offset < 0 or (args.remaining and args.paths):
        parser.error(
            "offset must be nonnegative; --remaining cannot be combined with --paths"
        )
    if not args.state:
        parser.error("--state is required for an inventory")
    previous = (
        json.loads(args.state.read_text(encoding="utf-8"))
        if args.state.exists()
        else None
    )
    if args.render_only:
        if args.query or not previous or previous.get("version") not in (2, 3):
            parser.error("--render-only requires a version 2 or 3 state and no --query")
        state = previous
        output = state["output"]
    else:
        if not args.query:
            parser.error("--query is required unless --render-only is used")
        query = json.loads(args.query.read_text(encoding="utf-8"))
        output, state = inventory(args.root, query, previous, refresh=args.refresh)
    delivery = {}
    if args.report:
        protected = [args.state, args.query] if args.query else [args.state]
        if any(args.report.resolve() == p.resolve() for p in protected):
            parser.error("report must not overwrite the query or state")
        if args.report.resolve().is_relative_to(Path(state["root"]).resolve()):
            parser.error("report must be outside the source tree")
    if not args.render_only:
        args.state.parent.mkdir(parents=True, exist_ok=True)
        write_json_atomic(args.state, state)
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        delivery = export_delivery(state, args.report)
    # Every mode keeps the same result envelope: counts and readiness must not
    # require another call simply because the caller also requested paths.
    summary = {
        key: output[key]
        for key in (
            "count",
            "untracked_count",
            "category_counts",
            "searched_records",
            "reused_records",
        )
    }
    summary["format"] = RESULT_FORMAT
    summary.update(
        {
            key: output[key]
            for key in (
                "scan_epoch",
                "scan_pending",
                "source_bytes_read",
                "evidence_scope",
            )
            if key in output
        }
    )
    summary["unresolved_count"] = len(output["unresolved"])
    summary["artifact"] = str(args.state.resolve())
    summary["query_id"] = output.get("query_id", query_identity(state["query"]))
    summary["earlier_scope_count"] = len(state.get("scope_changes", []))
    summary.update(
        {
            key: output[key]
            for key in ("missing_categories", "ready_to_render", "next_action")
        }
    )
    if args.paths:
        summary["paths"] = output["paths"][args.offset : args.offset + PAGE_RECORDS]
        summary["paths_next_offset"] = (
            args.offset + PAGE_RECORDS
            if args.offset + PAGE_RECORDS < len(output["paths"])
            else None
        )
    if args.remaining:
        summary["remaining"] = [
            {key: r[key] for key in ("path", "category", "unresolved", "evidence")}
            for r in output["unresolved"][args.offset : args.offset + PAGE_RECORDS]
        ]
        summary["next_offset"] = (
            args.offset + PAGE_RECORDS
            if args.offset + PAGE_RECORDS < len(output["unresolved"])
            else None
        )
    json_summaries = successful_json_summaries(state)
    if json_summaries:
        summary["json_summaries"], summary["json_summary_next_offset"] = (
            json_summary_page(json_summaries, args.offset)
        )
        summary["json_summary_count"] = len(json_summaries)
    if args.report:
        summary.update(delivery)
        if output["ready_to_render"]:
            summary["next_action"] = "deliver_report"
    print(json.dumps(summary, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, KeyError) as error:
        print(str(error), file=sys.stderr)
        sys.exit(1)
