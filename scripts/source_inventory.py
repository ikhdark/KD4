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
import tempfile
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
# A short remainder costs less inline than another --remaining call.
INLINE_REMAINING_RECORDS = 10
PRIOR_QUERY_LIMIT = 3
# Leave headroom below CreateProcessW's 32,767 UTF-16 code-unit limit.
GIT_COMMAND_UNITS = 24_000


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
        batches = []
        batch = []
        base_units = len(subprocess.list2cmdline([*args, "--"]).encode("utf-16-le")) // 2
        units = base_units
        for path in paths:
            path_units = len(
                subprocess.list2cmdline([f":(literal){path}"]).encode("utf-16-le")
            ) // 2 + 1
            if base_units + path_units > GIT_COMMAND_UNITS:
                raise ValueError(f"source path exceeds the git command-line budget: {path!r}")
            if batch and units + path_units > GIT_COMMAND_UNITS:
                batches.append(tuple(batch))
                batch = []
                units = base_units
            batch.append(path)
            units += path_units
        if batch:
            batches.append(tuple(batch))
        if len(batches) > 1:
            records = {}
            for batch in batches:
                records.update(repository_source_records(
                    repo_root, include_untracked=include_untracked, prune=prune,
                    paths=batch,
                ))
            return records
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
        "categories": sorted(
            (
                {**rule, "paths": [normalized_path(path) for path in rule["paths"]]}
                for rule in query["categories"]
            ),
            key=lambda rule: rule["name"],
        ),
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
        return {"type": "string", "length": len(value),
                "sha256": digest(value.encode("utf-8", errors="surrogatepass"))}
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
            pattern = normalized_path(pattern)
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
        rule = {**rule, "paths": [normalized_path(path) for path in rule["paths"]]}
        categories[name] = (
            rule,
            re.compile(rule["contains"]) if rule.get("contains") else None,
        )
    if not categories:
        raise ValueError("at least one category is required")
    return categories


def validate_control_paths(root, definition, **paths):
    """Control files must not become evidence that their next write invalidates."""
    root = root.resolve()
    patterns = [
        normalized_path(pattern)
        for rule in definition["categories"]
        for pattern in rule["paths"]
    ]
    candidates = {normalized_path(r["path"]) for r in definition.get("candidates", [])}
    for name, path in paths.items():
        for location in {path.absolute(), path.resolve()}:
            if not location.is_relative_to(root):
                continue
            relative = location.relative_to(root).as_posix()
            if any(part in PRUNE for part in PurePosixPath(relative).parts[:-1]):
                continue
            if relative in candidates or any(
                fnmatch.fnmatchcase(relative, pattern) for pattern in patterns
            ):
                raise ValueError(
                    f"{name} file overlaps selected sources: {relative}; "
                    "place it outside the discovery scope"
                )


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


def source_revision(path):
    """Cheap race/continuation guard; content hashes remain the evidence identity."""
    try:
        stat = path.lstat()
        return [stat.st_dev, stat.st_ino, stat.st_mode, stat.st_size,
                stat.st_mtime_ns, stat.st_ctime_ns]
    except FileNotFoundError:
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
    # Freeze the discovered scope as well as each selected file's revision.
    # A pending scan must never silently combine observations from two epochs.
    def selected_sources(sources):
        return {
            path: tracking for path, tracking in sources.items()
            if any(fnmatch.fnmatchcase(path, pattern)
                   for rule, _ in categories.values() for pattern in rule["paths"])
        }

    source_set = selected_sources(states)
    old_files = previous.get("files", {}) if previous.get("root") == str(root) else {}
    continuing = (
        not refresh
        and bool(previous.get("scan", {}).get("pending"))
        and query_identity(previous["query"]) == query_id
    )
    if continuing and previous["scan"].get("source_set") != source_set:
        raise ValueError("source set changed or legacy pending epoch; use --refresh")
    source_revisions = {}
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
        if not excluded:
            source_revisions[path] = source_revision(full_path)
            if continuing and previous["scan"].get("source_revisions", {}).get(path) != source_revisions[path]:
                raise ValueError(f"source changed during retained epoch: {path}; use --refresh")
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
        elif state == "deleted" and not exists:
            # Git records this tracked path as deleted in the working tree: an
            # explicit change with no content to classify. Report it as deleted
            # rather than as uncertain evidence; a path that disappears without
            # git's deletion record remains unresolved drift below.
            for name in sorted(selected):
                records[(path, name)] = {
                    "path": path,
                    "category": name,
                    "exists": False,
                    "tracking": state,
                    "evidence": None,
                    "unresolved": None,
                    "status": "deleted",
                    "decision": None,
                }
            continue
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
            files[path] = {"sha256": content_hash, "categories": entries,
                           "bytes": len(data) if data is not None else source_revisions[path][3]}

    for path, revision in source_revisions.items():
        if source_revision(root / path) != revision:
            raise ValueError(f"source changed during scan: {path}; use --refresh")
    if selected_sources(repository_source_records(
        root, prune=PRUNE, paths=discovery_paths(query),
    )) != source_set:
        raise ValueError("source set changed during scan; use --refresh")

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
    deleted = sorted({record["path"] for record in result if record["status"] == "deleted"})
    # Deleted paths are part of the selected scope, so they belong to the
    # snapshot identity; without any, the identity is unchanged.
    source_hash = digest(json.dumps(
        sorted(
            [[path, source_set.get(path), value["sha256"]]
             for path, value in files.items()]
            + [[path, "deleted", None] for path in deleted]
        ),
        ensure_ascii=False, separators=(",", ":"),
    ).encode("utf-8"))
    # Partial evidence has its own identity; completing it converges to the
    # same identity as a fresh full scan, regardless of batching or timestamps.
    epoch = digest(json.dumps(
        [query_id, source_hash, sorted(pending)],
        separators=(",", ":"),
    ).encode("utf-8"))
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
        "deleted": deleted,
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
    if output["ready_to_render"]:
        # Include negative matches: identical results alone do not establish
        # identical source inputs. Exclude timestamps, epochs and host paths.
        output["source_snapshot_sha256"] = source_hash
    state = {
        "version": 3,
        "root": str(root),
        "files": files,
        "records": result,
        "scan": {"epoch": epoch, "pending": pending,
                 "source_set": source_set, "source_revisions": source_revisions},
        "scope_changes": scope_changes,
        "query": query,
        "output": output,
    }
    if "review" in previous:
        state["review"] = {
            path: record for path, record in previous["review"].items()
            if path in files and record["sha256"] == files[path]["sha256"]
        }
        state["stale_review"] = {**previous.get("stale_review", {}), **{
            f"{path}@{record['sha256']}": record
            for path, record in previous["review"].items() if path not in state["review"]
        }}
        if previous.get("query") == query and previous.get("review_observations"):
            state["review_observations"] = previous["review_observations"]
        output["review_progress"] = review_progress(state)
    return output, state


def merge_review_ranges(ranges, size):
    merged = []
    for item in ranges:
        if (not isinstance(item, list) or len(item) != 2
                or any(type(value) is not int for value in item)
                or not 0 <= item[0] < item[1] <= size):
            raise ValueError("review ranges must be nonempty half-open byte ranges within the source")
    for item in sorted(ranges):
        if merged and item[0] <= merged[-1][1]:
            merged[-1][1] = max(merged[-1][1], item[1])
        else:
            merged.append(item.copy())
    return merged


def review_progress(state):
    """Declared reads are distinct from inventory matches and semantic decisions."""
    total = read = completed = 0
    next_batch = []
    budget = SUMMARY_PAGE_BYTES
    unknown = {r["path"] for r in state["records"]
               if r["status"] != "deleted" and r["path"] not in state["files"]}
    for path, source in sorted(state["files"].items()):
        size = source.get("bytes")
        if size is None:
            unknown.add(path)
            continue
        total += size
        record = state.get("review", {}).get(path, {})
        ranges = record.get("ranges", [])
        read += sum(end - start for start, end in ranges)
        completed += record.get("disposition") == "reviewed"
        cursor = 0
        for start, end in [*ranges, [size, size]]:
            if cursor < start and budget and len(next_batch) < PAGE_RECORDS:
                stop = min(start, cursor + budget)
                next_batch.append({"path": path, "sha256": source["sha256"],
                                   "start": cursor, "end": stop})
                budget -= stop - cursor
            cursor = end
    progress = {
        "evidence_kind": "reviewer_declared_coverage_not_host_verified_semantics",
        "inventoried_files": len(state["files"]), "reviewed_files": completed,
        "unresolved_files": len(set(state["files"]) | unknown) - completed,
        "unavailable_files": sorted(unknown),
        "source_bytes": total, "read_bytes": read, "remaining_bytes": total - read,
        "minimum_read_batches": (total - read + SUMMARY_PAGE_BYTES - 1) // SUMMARY_PAGE_BYTES,
        "next_batch": next_batch,
        "complete": not unknown and completed == len(state["files"]),
    }
    observations = sorted(state.get("review_observations", {}).values(),
                          key=lambda row: row["ordinal"])
    if observations:
        measured_bytes = sum(row["new_read_bytes"] for row in observations)
        elapsed_ms = sum(row["elapsed_ms"] for row in observations)
        forecast = {
            "basis": "caller_observed_batches_same_query_not_a_completion_guarantee",
            "observed_batches": len(observations),
            "observed_elapsed_ms": elapsed_ms,
            "observed_new_read_bytes": measured_bytes,
            "observed_reviewed_files": sum(row["new_reviewed_files"] for row in observations),
        }
        if measured_bytes:
            forecast["estimated_remaining_read_ms"] = (
                (total - read) * elapsed_ms + measured_bytes - 1
            ) // measured_bytes
            forecast["ms_per_mib"] = elapsed_ms * 1048576 // measured_bytes
        for metric in ("model_input_tokens", "model_output_tokens"):
            measured = [row for row in observations if metric in row]
            if measured:
                forecast[metric] = sum(row[metric] for row in measured)
                forecast[f"{metric}_observed_batches"] = len(measured)
        if len(observations) >= 2:
            previous, latest = observations[-2:]
            if previous["elapsed_ms"] and latest["elapsed_ms"]:
                forecast["read_throughput_declining"] = (
                    latest["new_read_bytes"] * previous["elapsed_ms"]
                    < previous["new_read_bytes"] * latest["elapsed_ms"]
                )
        progress["forecast"] = forecast
    return progress


def update_review(state, update):
    """Atomically persisted by main; replaying identical updates is idempotent."""
    if update.get("scan_epoch") != state["scan"]["epoch"]:
        raise ValueError("review update belongs to a different scan epoch")
    observation = update.get("observation")
    observation_key = None
    if observation is not None:
        if not isinstance(observation, dict) or not isinstance(observation.get("id"), str) or not observation["id"].strip():
            raise ValueError("review observation requires a nonempty batch id")
        for key in ("elapsed_ms", "model_input_tokens", "model_output_tokens"):
            if key == "elapsed_ms" or key in observation:
                if type(observation.get(key)) is not int or observation[key] < 0:
                    raise ValueError(f"observation {key} must be a nonnegative integer")
        observation_key = f"{update['scan_epoch']}:{observation['id']}"
        update_hash = digest(json.dumps(update, sort_keys=True, separators=(",", ":")).encode("utf-8"))
        prior_observation = state.get("review_observations", {}).get(observation_key)
        if prior_observation is not None:
            if prior_observation["update_sha256"] != update_hash:
                raise ValueError("review batch id already committed with different contents")
            return state
    before = review_progress(state)
    # Validate the entire update before changing the caller's retained state.
    state = json.loads(json.dumps(state))
    review = state.setdefault("review", {})
    for record in update.get("records", []):
        path = normalized_path(record["path"])
        source = state["files"].get(path)
        if source is None or source["sha256"] != record.get("sha256"):
            raise ValueError(f"review source is missing or stale: {path}")
        if "bytes" not in source:
            raise ValueError("legacy inventory lacks source lengths; refresh before recording coverage")
        previous = review.get(path, {})
        ranges = merge_review_ranges([*previous.get("ranges", []), *record.get("ranges", [])], source["bytes"])
        disposition = record.get("disposition", previous.get("disposition", "unreviewed"))
        reason = record.get("reason", previous.get("reason", ""))
        findings = record.get("findings", previous.get("findings", []))
        unresolved = record.get("unresolved", previous.get("unresolved", []))
        if disposition not in ("unreviewed", "reviewed", "unresolved"):
            raise ValueError("review disposition must be unreviewed, reviewed, or unresolved")
        if not isinstance(reason, str) or (disposition != "unreviewed" and not reason.strip()):
            raise ValueError("semantic review dispositions require an explicit reason")
        if any(not isinstance(values, list) or any(not isinstance(v, str) or not v.strip() for v in values)
               for values in (findings, unresolved)):
            raise ValueError("findings and unresolved must be lists of nonempty references")
        covered = sum(end - start for start, end in ranges)
        if disposition == "reviewed" and (covered != source["bytes"] or unresolved):
            raise ValueError("reviewed requires complete read coverage and no unresolved obligations")
        review[path] = {"sha256": source["sha256"], "ranges": ranges,
                        "disposition": disposition, "reason": reason,
                        "findings": findings, "unresolved": unresolved}
    if observation_key is not None:
        after = review_progress(state)
        measurement = {
            "ordinal": len(state.get("review_observations", {})),
            "update_sha256": update_hash,
            "elapsed_ms": observation["elapsed_ms"],
            "new_read_bytes": max(0, after["read_bytes"] - before["read_bytes"]),
            "new_reviewed_files": max(0, after["reviewed_files"] - before["reviewed_files"]),
        }
        for key in ("model_input_tokens", "model_output_tokens"):
            if key in observation:
                measurement[key] = observation[key]
        state.setdefault("review_observations", {})[observation_key] = measurement
    state["output"]["review_progress"] = review_progress(state)
    return state


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
    if output.get("deleted"):
        lines.extend(
            [
                "## Deleted in the working tree",
                "",
                "Tracked paths that git reports as deleted; they have no content "
                "to classify and are not part of the included counts.",
                "",
            ]
            + [f"- {code(p)}" for p in output["deleted"]]
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
    if "review_progress" in output:
        progress = output["review_progress"]
        lines.extend(["", "## Semantic review coverage", "",
                      "Reviewer declarations, not a host assertion that source semantics are correct.",
                      f"Reviewed files: {progress['reviewed_files']}; unresolved: {progress['unresolved_files']}; unread bytes: {progress['remaining_bytes']}."])
        for path, record in sorted(state.get("review", {}).items()):
            lines.append(f"- {code(path)}: {code(record['disposition'])}; {html.escape(record['reason'])}; findings: {code(', '.join(record['findings']))}; unresolved: {code(', '.join(record['unresolved']))}")
        if state.get("stale_review"):
            lines.append(f"Stale review records preserved in canonical JSON: {len(state['stale_review'])}.")
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
        "review": {
            "invocation": "--state STATE --review REVIEW_JSON [--report REPORT]; no source rescan. Serialize updates to a task-owned state.",
            "input": {"scan_epoch": "exact retained scan_epoch", "records": [{"path": "selected source path", "sha256": "exact source hash", "ranges": "half-open UTF-8 byte ranges explicitly read by the reviewer", "disposition": "unreviewed | unresolved | reviewed", "reason": "required for semantic dispositions", "findings": "finding reference strings", "unresolved": "remaining obligation strings"}]},
            "semantics": "Send records=[] to initialize coverage and obtain a bounded next_batch. Updates merge ranges idempotently and are persisted atomically before publication. Reviewed requires complete ranges and no unresolved obligations; this is a reviewer declaration, not semantic proof. Inventory readiness never implies review completion. Changed sources invalidate active coverage but preserve stale review records. Legacy states without byte lengths need a refresh.",
            "observation": "Optional input observation={id, elapsed_ms, model_input_tokens?, model_output_tokens?} records measured batch cost, never estimated or cumulative account usage. IDs are unique per scan; identical replays are no-ops and conflicting reuse fails before mutation.",
            "forecast": "review_progress reports remaining_bytes, minimum_read_batches (16 KiB byte budget), next_batch, unavailable_files, and reviewed/unresolved counts. Optional observations add elapsed cost, ms_per_mib, extrapolated remaining read time, and declining throughput. These are historical same-query observations, not semantic completion guarantees or authority to narrow scope. Token counts are request-processing counts, not unique context size or billing.",
        },
        "snapshot": "Scans reject source/revision drift within a retained epoch. Complete results include source_snapshot_sha256 over all selected source hashes, including negative matches. This identifies captured evidence, not an atomic filesystem snapshot; --render-only replays it without reading live sources.",
        "invocation": {
            "file": "python -X utf8 scripts/source_inventory.py --root . --query QUERY --state STATE --report REPORT",
            "powershell_stdin": "& {\n  $OutputEncoding = [System.Text.UTF8Encoding]::new($false)\n  @'\n{\"categories\":[{\"name\":\"templates\",\"paths\":[\"templates/*.md\",\"*/templates/*.md\"],\"verification\":\"path\"}]}\n'@ | python -X utf8 scripts/source_inventory.py --root . --query -\n}\nif ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }",
            "stdin": "--query - reads UTF-8 JSON from stdin (an optional UTF-8 BOM is accepted). The PowerShell example scopes OutputEncoding to the script block; it changes no global setting.",
            "continuation": "Reuse the query with --root ROOT --query QUERY --state ARTIFACT (or pipe it with --query -). Add --report REPORT to deliver the updated report; explicit --state does not imply a report. ARTIFACT is the returned state path. Omitting --state starts a new independent run, not a continuation.",
            "refresh": "Add --refresh to a scan with the retained state to start a new epoch and read current sources instead of continuing captured evidence.",
        },
        "workflow": {
            "default": "describe -> scan -> final when the scan supplies sufficient evidence. If tool discovery is necessary, locate the entrypoint and read this contract in the same execution cell; do not return to the model merely to request --describe or load a second inventory tool. Return for query decisions that require this contract. Build the query directly from the requested categories and known scope; the initial scan performs file discovery, so do not first list the repository or probe example directories.",
            "scope": "Use known roots for a restricted request. For repository-wide filename/rule inventories with unknown layout, use layout-independent globs: *.rs matches Rust files at any depth; templates/*.md plus */templates/*.md covers root and nested templates directories; models.json plus */models.json covers that basename at any depth. Examples are illustrative, not a complete semantic inventory or permission to broaden a restricted request. Use contains for content criteria and retain runtime verification for activation claims.",
            "classification": "For prompt/guidance inventories, separate dedicated assets from keyword-matched source, documentation, configuration and test candidates. A mention is not a definition. Include known guidance directories explicitly so keyword-free skill references are retained; do not replace a text filter with every *.md or *.txt file, which also selects animation frames and patch fixtures. Confirm inline definitions with targeted evidence before promoting candidates to the primary list.",
            "scope_changes": "Choose inclusion rules before scanning. Revise them only for a demonstrated coverage gap or query error; repair that specific rule without changing unrelated categories. Reuse retained evidence and deliver sufficient counts/report links rather than reopening state or dumping complete categories to polish the answer.",
            "inspection_exception": "Inspect before scanning only when a concrete uncertainty cannot be expressed by query rules and its answer would change category coverage or verification. Name that uncertainty and use the smallest targeted inspection, not general repository orientation. Resolve pending scans and runtime evidence after the scan when required; do not force final delivery from insufficient evidence.",
        },
        "globs": "fnmatch.fnmatchcase on normalized repository-relative POSIX paths; case-sensitive. Both * and ? can match /, so dir/*.rs also matches dir/nested/file.rs. Backslashes and leading ./ are normalized.",
        "contains": "Optional Python regular expression searched with re.search over UTF-8 file text, not filenames; case-sensitive unless flags such as (?i) are supplied. For keyword searches prefer simple alternation, e.g. (?i)prompt|instruction|guidance|template; use re.escape for literal punctuation. Non-UTF-8 content is unresolved when text matching is required.",
        "budget": {
            "max_file_bytes": MAX_FILE_BYTES,
            "max_scan_bytes": MAX_SCAN_BYTES,
            "continuation": "When scan_pending > 0, continue with the returned artifact and the same query. Oversized files remain unresolved; continuation does not lift the per-file limit. A continuation returning no newly required inventory evidence is unnecessary unless needed to establish completion.",
        },
        "query": {
            "required_categories": "Optional names whose coverage must be retained; defaults to all declared categories. Omit unless overriding that default. Raw queries and retained state.query may omit this key; adding a category does not require appending to an absent list.",
            "categories": [
                {
                    "name": "unique category name",
                    "paths": ["repository-relative globs; backslash separators and leading ./ are normalized"],
                    "verification": "path | runtime (default runtime)",
                    "contains": "optional Python regular expression",
                    "json_summary": "optional boolean; omit for file listings. Enable only when JSON structure is needed, in a JSON-only category; TOML/YAML are not JSON. Exposes fields, types and lengths, never string bodies",
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
            "reuse": "--query also accepts an earlier canonical delivery JSON (a canonical_paths file, for example from prior_queries) and reproduces its exact scope and query_id; its review decisions are not reused.",
        },
        "example": {
            "categories": [
                {
                    "name": "templates",
                    "paths": ["templates/*.md", "*/templates/*.md"],
                    "verification": "path",
                },
                {
                    "name": "catalog",
                    "paths": ["models.json", "*/models.json"],
                    "verification": "path",
                },
                {
                    "name": "guidance_assets",
                    "paths": [
                        "AGENTS.md", "*/AGENTS.md", "SKILL.md", "*/SKILL.md",
                        "skills/*.md", "*/skills/*.md",
                    ],
                    "verification": "path",
                },
                {
                    "name": "related_text_candidates",
                    "paths": ["*.rs", "*.md", "*.txt", "*.json", "*.toml", "*.yaml", "*.yml"],
                    "contains": "(?i)prompt|instruction|guidance|template",
                    "verification": "path",
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
            "scan_epoch": "SHA-256 of query identity, selected source hashes and pending paths; changes as partial evidence advances and converges to the fresh full-scan identity",
            "source_snapshot_sha256": "complete-scan identity over selected paths, tracking and source hashes, including negative matches; omitted for legacy or partial states",
            "scan_pending": "number of files awaiting another scan batch",
            "source_bytes_read": "source bytes read in this batch",
            "evidence_scope": "captured evidence freshness and scope",
            "unresolved_count": "records requiring inspection",
            "deleted_count": "tracked paths git reports deleted in the working tree; listed in the report and canonical JSON, never unresolved; present when nonzero",
            "prior_queries": f"up to {PRIOR_QUERY_LIMIT} earlier delivered queries for this root with a different scope (query_id, categories, count, ready_to_render, canonical_paths); present when any exist. Pass canonical_paths as --query to reproduce that scope exactly",
            "missing_categories": "required undeclared categories",
            "earlier_scope_count": "retained scope changes",
            "ready_to_render": "true only for the declared scope",
            "next_action": "resolve_remaining | render | deliver_report",
            "paths": "with --paths: page of included tracked paths, never a replacement for this envelope",
            "paths_next_offset": "next path offset or null",
            "remaining": f"with --remaining: page of path, category, unresolved reason and bounded evidence; returned automatically when 1-{INLINE_REMAINING_RECORDS} records remain",
            "next_offset": "next unresolved offset or null",
            "json_summaries": [
                {
                    "path": "source path",
                    "category": "category",
                    "status": "matched | verified",
                    "tracking": "tracked | untracked",
                    "sha256": "source hash",
                    "structure": "bounded tree: type, length, fields/items, omitted; newly captured string summaries include SHA-256 of decoded UTF-8 (surrogatepass), never bodies. Older retained summaries may lack value hashes",
                }
            ],
            "json_summary_count": "successful JSON records; present when nonzero",
            "json_summary_next_offset": "next summary offset or null",
            "report": "with --report or defaulted state/report: immutable Markdown path",
            "canonical_paths": "immutable JSON path",
            "delivery_sha256": "hash of canonical JSON bytes",
        },
        "paging": f"Use --state STATE --render-only --offset N without rescanning. Paths and remaining pages hold {PAGE_RECORDS} records; JSON summaries also have a {SUMMARY_PAGE_BYTES}-byte target. Each next-offset field advances its own list.",
        "control_files": "Without --state, each scan creates a unique task-owned state.json under the system temporary directory. Default reports are permanent under ~/.cache/codex/source-inventory/<scan_epoch>/inventory.md, with the directory derived from query and scanned-file hashes. Reports use repository-relative paths and are identical across isolated copies. State retains the actual source root and is never selected by query hash. Explicit --state preserves explicit-path behavior: no report unless requested. --render-only requires explicit --state and no query. Query files and state must be distinct and outside selected sources; stdin has no query path. Reports must be outside the source tree. Pruned directories are outside discovery scope.",
        "delivery": "Canonical JSON contains all paths, categories, json_summaries, unresolved/excluded records and prior scope. Deliver sufficient returned counts and report/canonical-path links without another call. When exec direct delivery is available and the scope and answer format are settled before scanning, start the scan cell with // @exec: {\"deliver\": true}. Await all work and check command success and streams_complete before parsing stdout. Deliver only when ready_to_render is true, scan_pending and unresolved_count are zero, missing_categories is empty, next_action is deliver_report, and the returned deliverables satisfy the user's request. Emit only the final answer with counts, report links, and material limitations; do not send the complete result envelope back to the model just to reformat it. If evidence is incomplete or a new decision is required, emit bounded diagnostic evidence and await yield_control() instead of finalizing. State is internal; use --render-only with --paths or --remaining only for newly required evidence, not to re-derive returned counts. A ready result proves only the declared query, not that its scope answers the entire task. When this contract supplies the needed facts, do not read or search source_inventory.py.",
    }


def report_directory(state):
    """Permanent content-addressed reports, outside the source checkout."""
    return Path.home() / ".cache" / "codex" / "source-inventory" / state["scan"]["epoch"]


def recent_index_path():
    """Append-only index of delivered queries, outside every source checkout."""
    return Path.home() / ".cache" / "codex" / "source-inventory" / "recent.jsonl"


def query_from_delivery(document):
    """A canonical delivery reproduces its exact scope; decisions are not reused."""
    profile = document["profile"]
    return {
        "categories": profile["categories"],
        "required_categories": profile["required_categories"],
        "candidates": profile["candidates"],
    }


def prior_queries(root, query_id):
    """Most recent delivered queries for this root whose scope differs."""
    try:
        lines = recent_index_path().read_text(encoding="utf-8").splitlines()
    except OSError:
        return []
    prior, seen = [], {query_id}
    for line in reversed(lines):
        try:
            entry = json.loads(line)
        except ValueError:
            continue
        if entry.get("root") != root or entry.get("query_id") in seen:
            continue
        seen.add(entry["query_id"])
        prior.append(
            {
                key: entry.get(key)
                for key in (
                    "query_id",
                    "categories",
                    "count",
                    "ready_to_render",
                    "canonical_paths",
                )
            }
        )
        if len(prior) == PRIOR_QUERY_LIMIT:
            break
    return prior


def record_delivery(state, delivery):
    """Index the delivery so a later run of the same request can reuse its scope."""
    output = state["output"]
    entry = {
        "root": state["root"],
        "query_id": output.get("query_id", query_identity(state["query"])),
        "categories": [
            rule["name"] for rule in query_profile(state["query"])["categories"]
        ],
        "count": output["count"],
        "ready_to_render": output["ready_to_render"],
        "canonical_paths": delivery["canonical_paths"],
    }
    index = recent_index_path()
    index.parent.mkdir(parents=True, exist_ok=True)
    with index.open("a", encoding="utf-8") as stream:
        stream.write(json.dumps(entry, ensure_ascii=False) + "\n")


def export_delivery(state, report):
    """Content-addressed delivery survives later state/report revisions."""
    # Reports describe repository-relative paths and can be shared by isolated
    # copies. The task-owned continuation state retains the actual source root.
    state = {**state, "root": "."}
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
    if output.get("deleted"):
        document["deleted"] = output["deleted"]
    if "review" in state:
        document["review"] = state["review"]
        document["review_progress"] = output["review_progress"]
        if state.get("stale_review"):
            document["stale_review"] = state["stale_review"]
        if state.get("review_observations"):
            document["review_observations"] = state["review_observations"]
    if "source_snapshot_sha256" in output:
        document["source_snapshot_sha256"] = output["source_snapshot_sha256"]
        document["sources"] = [
            {"path": path, "tracking": state["scan"]["source_set"].get(path),
             "sha256": value["sha256"]}
            for path, value in sorted(state["files"].items())
        ]
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
    parser.add_argument("--query", type=Path, help="Query JSON file, or - for UTF-8 stdin")
    parser.add_argument("--review", type=Path, help="Apply hash-bound review coverage JSON to an explicit retained --state, without rescanning")
    parser.add_argument(
        "--state",
        type=Path,
        help="Task-owned retained records; omit to create a unique temporary run",
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
        help="Write a Markdown report; defaults to inventory.md only when --state is omitted",
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
    if args.review and (not args.state or args.query or args.render_only or args.refresh or args.instructions or args.describe):
        parser.error("--review requires --state and cannot scan, refresh, discover, or render-only")
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
    query_path = args.query if args.query != Path("-") else None
    default_report = not args.state and not args.report
    if args.render_only and not args.state:
        parser.error("--render-only requires an explicit --state")
    if args.render_only and args.query:
        parser.error("--render-only requires a version 2 or 3 state and no --query")
    if not args.render_only and not args.review:
        if not args.query:
            parser.error("--query is required unless --render-only is used")
        if query_path is not None:
            query = json.loads(query_path.read_text(encoding="utf-8"))
        else:
            text = getattr(sys.stdin, "buffer", sys.stdin).read()
            if isinstance(text, bytes):
                text = text.decode("utf-8-sig")
            query = json.loads(text)
        if (
            isinstance(query, dict)
            and query.get("format") == RESULT_FORMAT
            and "profile" in query
        ):
            query = query_from_delivery(query)
        if not args.state:
            run_dir = Path(tempfile.mkdtemp(prefix="source-inventory-"))
            args.state = run_dir / "state.json"
            if not args.report:
                args.report = run_dir / "inventory.md"
    if query_path and args.state.resolve() == query_path.resolve():
        parser.error("state must not overwrite the query")
    previous = (
        json.loads(args.state.read_text(encoding="utf-8"))
        if args.state.exists()
        else None
    )
    if args.review:
        if not previous or previous.get("version") != 3:
            parser.error("--review requires a version 3 inventory state")
        if args.review.resolve() == args.state.resolve():
            parser.error("review input must not overwrite state")
        state = update_review(previous, json.loads(args.review.read_text(encoding="utf-8")))
        output = state["output"]
    elif args.render_only:
        if args.query or not previous or previous.get("version") not in (2, 3):
            parser.error("--render-only requires a version 2 or 3 state and no --query")
        state = previous
        output = state["output"]
    else:
        controls = {"state": args.state}
        if query_path is not None:
            controls["query"] = query_path
        validate_control_paths(args.root, query, **controls)
        output, state = inventory(args.root, query, previous, refresh=args.refresh)
    if default_report:
        args.report = report_directory(state) / "inventory.md"
    delivery = {}
    if args.report:
        protected = [args.state, query_path] if query_path else [args.state]
        if args.review:
            protected.append(args.review)
        if any(args.report.resolve() == p.resolve() for p in protected):
            parser.error("report must not overwrite the query or state")
        if args.report.resolve().is_relative_to(Path(state["root"]).resolve()):
            parser.error("report must be outside the source tree")
    if not args.render_only:
        args.state.parent.mkdir(parents=True, exist_ok=True)
        write_json_atomic(args.state, state)
    prior = []
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        delivery = export_delivery(state, args.report)
        # Only default deliveries live in the shared cache; an explicit report
        # path belongs to its caller and is not indexed.
        if default_report and not args.render_only:
            prior = prior_queries(
                state["root"],
                output.get("query_id", query_identity(state["query"])),
            )
            try:
                record_delivery(state, delivery)
            except OSError:
                # The index only aids later reuse; delivery already succeeded.
                pass
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
                "source_snapshot_sha256",
            )
            if key in output
        }
    )
    summary["unresolved_count"] = len(output["unresolved"])
    if "review_progress" in output:
        summary["review_progress"] = output["review_progress"]
    if output.get("deleted"):
        summary["deleted_count"] = len(output["deleted"])
    if prior:
        summary["prior_queries"] = prior
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
    elif 0 < len(output["unresolved"]) <= INLINE_REMAINING_RECORDS:
        summary["remaining"] = [
            {key: r[key] for key in ("path", "category", "unresolved", "evidence")}
            for r in output["unresolved"]
        ]
        summary["next_offset"] = None
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
