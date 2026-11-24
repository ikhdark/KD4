"""Read Cargo lockfile graphs, run declared JSON migrations, or assemble a diagnostic ZIP."""
from __future__ import annotations

import argparse
import hashlib
import json
import ntpath
import re
import tomllib
import zipfile
from pathlib import Path

FILE_LIMIT = 16 * 1024 * 1024
BUNDLE_LIMIT = 64 * 1024 * 1024


def read_bytes(path, limit=FILE_LIMIT):
    with path.open("rb") as stream:
        data = stream.read(limit + 1)
    if len(data) > limit:
        raise ValueError(f"{path}: exceeds {limit} bytes")
    return data


def read_json(path):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"duplicate JSON key: {key!r}")
            result[key] = value
        return result

    def invalid_constant(value):
        raise ValueError(f"invalid JSON number: {value}")

    return json.loads(read_bytes(path), object_pairs_hook=unique, parse_constant=invalid_constant)


def object_value(value):
    if not isinstance(value, dict):
        raise ValueError("expected a JSON object")
    return value


def pointer_tokens(pointer):
    if not isinstance(pointer, str) or (pointer and not pointer.startswith("/")):
        raise ValueError("expected a JSON Pointer")
    if re.search(r"~(?![01])", pointer):
        raise ValueError("invalid JSON Pointer escape")
    return [part.replace("~1", "/").replace("~0", "~") for part in pointer.split("/")[1:]]


def migrate(doc, recipe):
    import copy

    object_value(recipe)
    if set(recipe) != {"from", "to", "operations"}:
        raise ValueError("recipe must contain from, to and operations")
    before, after, operations = (recipe[k] for k in ("from", "to", "operations"))
    if not (type(before) is type(after) is int and 0 <= before < after
            and isinstance(operations, list) and len(operations) <= 1024):
        raise ValueError("invalid migration version or operation list")
    result = copy.deepcopy(object_value(doc))
    if type(result.get("schema_version")) is not int or result["schema_version"] != before:
        raise ValueError("input schema_version does not match recipe from")

    def target(pointer):
        parts = pointer_tokens(pointer)
        if not parts or parts[0] == "schema_version":
            raise ValueError("recipe cannot edit the root or schema_version")
        node = result
        for part in parts[:-1]:
            node = object_value(node)[part]
        return object_value(node), parts[-1]

    for row in operations:
        op = object_value(row).get("op")
        fields = {"add": {"value"}, "replace": {"value"}, "remove": set(), "move": {"from"}}
        if not isinstance(op, str) or op not in fields or set(row) != ({"op", "path"} | fields[op]):
            raise ValueError("invalid migration operation or fields")
        parent, key = target(row["path"])
        if op in ("add", "move") and key in parent:
            raise ValueError(f"destination already exists: {row['path']}")
        if op in ("remove", "replace") and key not in parent:
            raise ValueError(f"target is missing: {row['path']}")
        if op == "move":
            source, name = target(row["from"])
            source_parts = pointer_tokens(row["from"])
            if pointer_tokens(row["path"])[:len(source_parts)] == source_parts:
                raise ValueError("cannot move an object into itself")
            parent[key] = source.pop(name)
        elif op == "remove":
            del parent[key]
        else:
            parent[key] = row["value"]
    result["schema_version"] = after
    return result


def cargo_graph(doc):
    if doc.get("version") not in (3, 4) or "package" not in doc:
        raise ValueError("expected a Cargo v3/v4 lockfile with packages")
    packages = {}
    names = {}
    for row in doc["package"]:
        key = (row["name"], row["version"], row.get("source", ""))
        if not all(isinstance(field, str) for field in key) or key in packages:
            raise ValueError(f"invalid or duplicate Cargo package: {key}")
        if "replace" in row:
            raise ValueError("legacy Cargo replacement records are unsupported")
        packages[key] = row
        names.setdefault(key[0], []).append(key)

    def resolve(reference):
        pieces = reference.split(" ", 2)
        matches = names.get(pieces[0], ())
        if len(pieces) >= 2:
            matches = [key for key in matches if key[1] == pieces[1]]
        if len(pieces) == 3:
            source = pieces[2]
            if not (source.startswith("(") and source.endswith(")")):
                raise ValueError(f"invalid Cargo source qualifier: {reference}")
            matches = [key for key in matches if key[2] == source[1:-1]]
        if len(matches) != 1:
            raise ValueError(f"unresolved or ambiguous Cargo dependency: {reference}")
        return matches[0]

    return {key: (row.get("checksum"), {resolve(dep) for dep in row.get("dependencies", [])})
            for key, row in packages.items()}


def cargo_diff(left, right):
    a, b = cargo_graph(left), cargo_graph(right)
    changed = []
    for key in sorted(a.keys() & b.keys()):
        if a[key] == b[key]:
            continue
        checksum_a, deps_a = a[key]
        checksum_b, deps_b = b[key]
        row = {"package": key, "dependencies_added": sorted(deps_b - deps_a),
               "dependencies_removed": sorted(deps_a - deps_b)}
        if checksum_a != checksum_b:
            row["checksum"] = {"before": checksum_a, "after": checksum_b}
        changed.append(row)
    return {"added": sorted(b.keys() - a.keys()), "removed": sorted(a.keys() - b.keys()),
            "changed": changed}


def assemble(entries, output):
    entries = object_value(entries)
    if not 1 <= len(entries) <= 128:
        raise ValueError("bundle requires between 1 and 128 explicit files")
    manifest, data = {}, {}
    names = {"manifest.json"}
    total = 0
    for name, filename in sorted(entries.items()):
        parts = name.split("/")
        normal = name.lower()
        if (len(name) > 240 or any(not re.fullmatch(r"[A-Za-z0-9._ -]+", p)
                                   or p in (".", "..") or ntpath.isreserved(p) for p in parts)
                or any(normal == n or normal.startswith(n + "/") or n.startswith(normal + "/") for n in names)):
            raise ValueError(f"invalid or colliding bundle member: {name!r}")
        names.add(normal)
        path = Path(filename)
        if not path.is_absolute() or not path.is_file():
            raise ValueError("bundle entry sources must be explicitly named absolute files")
        content = read_bytes(path, min(FILE_LIMIT, BUNDLE_LIMIT - total))
        total += len(content)
        data[name] = content
        manifest[name] = {"bytes": len(content), "sha256": hashlib.sha256(content).hexdigest()}
    result = {"schema_version": 1, "files": manifest}
    with zipfile.ZipFile(output, "x", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("manifest.json", json.dumps(result, indent=2, sort_keys=True) + "\n")
        for name, content in data.items():
            archive.writestr(name, content)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="operation", required=True)
    lock = commands.add_parser("lock-diff", help="compare complete Cargo v3/v4 lockfiles")
    lock.add_argument("before", type=Path)
    lock.add_argument("after", type=Path)
    migration = commands.add_parser("migrate", help="run a versioned JSON object recipe without modifying the input")
    migration.add_argument("input", type=Path)
    migration.add_argument("recipe", type=Path)
    bundle = commands.add_parser("bundle", help="archive only explicitly selected files")
    bundle.add_argument("entries", type=Path, help="JSON object mapping archive names to absolute files")
    bundle.add_argument("output", type=Path, help="new ZIP path (must not exist)")
    args = parser.parse_args()
    try:
        if args.operation == "lock-diff":
            result = cargo_diff(*(tomllib.loads(read_bytes(path).decode("utf-8"))
                                  for path in (args.before, args.after)))
        elif args.operation == "migrate":
            result = migrate(read_json(args.input), read_json(args.recipe))
        else:
            result = assemble(read_json(args.entries), args.output)
    except (OSError, KeyError, TypeError, ValueError) as error:
        parser.exit(1, f"{parser.prog}: {error}\n")
    print(json.dumps(result, sort_keys=True, allow_nan=False))


if __name__ == "__main__":
    main()
