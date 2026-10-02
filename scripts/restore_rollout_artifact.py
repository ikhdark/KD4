#!/usr/bin/env python3
"""Restore one deleted-file undo record without overwriting existing work."""

import argparse
import hashlib
import json
from pathlib import Path

try:
    from scripts.rollout_snapshot import read_rollout_records
except ImportError:
    from rollout_snapshot import read_rollout_records


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rollout", required=True, type=Path)
    parser.add_argument("--call-id", required=True, help="patch_apply_end call_id")
    parser.add_argument("--deleted-file", required=True, help="Exact path key in the patch changes")
    parser.add_argument("--output", required=True, type=Path, help="New destination; must not exist")
    args = parser.parse_args(argv)
    matches = []
    for record, _ in read_rollout_records(args.rollout):
        payload = record.get("payload", {})
        if (record.get("type") == "event_msg" and payload.get("type") == "patch_apply_end"
                and payload.get("call_id") == args.call_id):
            change = payload.get("changes", {}).get(args.deleted_file)
            if isinstance(change, dict) and change.get("type") == "delete":
                matches.append(change["content"])
    if len(matches) != 1:
        raise ValueError(f"expected one deleted-file record, found {len(matches)}")
    data = matches[0].encode("utf-8")
    with args.output.open("xb") as destination:
        destination.write(data)
    print(json.dumps({"output": str(args.output.resolve()), "bytes": len(data),
                      "sha256": hashlib.sha256(data).hexdigest()}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
