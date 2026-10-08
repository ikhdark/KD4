"""Opt-in, content-addressed products for the canonical rollout audit.

Always authenticate live inputs before lookup. Stored reports are historical
observations, not refreshed validation receipts or execution authority.
"""

from __future__ import annotations

import contextlib
import hashlib
import json
import platform
import re
import sys
from pathlib import Path

try:
    from scripts.atomic_json import write_bytes_atomic, write_json_atomic
    from scripts import rollout_snapshot
except ImportError:
    from atomic_json import write_bytes_atomic, write_json_atomic
    import rollout_snapshot

MAX_CAPTURE_BYTES = 64 * 1024 * 1024
MAX_REPORT_BYTES = 64 * 1024 * 1024
MAX_DECODED_WIRE_BYTES = 8 * 1024 * 1024
MAX_DECODED_RECORDS = 100_000


def _bytes(value):
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), allow_nan=False
    ).encode()


def _sha(data):
    return hashlib.sha256(data).hexdigest()


def analyzer_identity():
    # Explicit transitive local analysis dependencies, not just the CLI version.
    names = (
        "rollout_audit_cache.py",
        "rollout_snapshot.py",
        "atomic_json.py",
        "kd4_turn_latency_audit.py",
        "kd4_timing_analysis.py",
        "kd4_first_useful_action_analysis.py",
        "kd4_session_diagnostics.py",
    )
    return {name: _sha(Path(__file__).with_name(name).read_bytes()) for name in names}


def _read_entry(root, identity):
    try:
        index = root / "inputs" / f"{identity}.json"
        if index.is_symlink() or index.stat().st_size > 4096:
            return None
        entry = json.loads(index.read_bytes())
        digest = entry.get("report_sha256")
        if (
            entry.get("input_sha256") != identity
            or not isinstance(digest, str)
            or not re.fullmatch(r"[0-9a-f]{64}", digest)
        ):
            return None
        path = root / "reports" / f"{digest}.json"
        if path.is_symlink() or path.stat().st_size > MAX_REPORT_BYTES:
            return None
        data = path.read_bytes()
        if _sha(data) != digest:
            return None
        report = json.loads(data)
        if (
            not isinstance(report, dict)
            or report.get("inputProvenance", {}).get("sha256") != identity
        ):
            return None
        return report, path, len(data)
    except (OSError, ValueError, TypeError, AttributeError):
        return None


def analyze_cached(
    analyze,
    source,
    repo_root,
    *,
    cache_dir,
    refresh=False,
    include_tokens=True,
    runner_evidence=None,
    startup_log=None,
    diagnostic_evidence=None,
):
    options = dict(
        include_tokens=include_tokens,
        runner_evidence=runner_evidence,
        startup_log=startup_log,
        diagnostic_evidence=diagnostic_evidence,
    )
    # This optional runner input opens another mutable file during analysis.
    # Until that owner accepts captured bytes, do not pretend its path is content.
    if runner_evidence and runner_evidence.get("providerRequestsPath"):
        report = analyze(source, repo_root, **options)
        report["analysisCache"] = {
            "status": "bypassed",
            "reason": "external provider request file",
        }
        return report
    source = rollout_snapshot.existing_rollout_path(source) if source else None
    files = (
        ([source] if source.is_file() else rollout_snapshot.discover_rollouts(source))
        if source
        else []
    )
    root = Path(cache_dir).resolve()
    captured, payloads, payload_ids = {}, {}, []
    decoded = {}
    decoded_wire_bytes = decoded_records = 0
    with contextlib.ExitStack() as stack:
        captured_bytes = 0
        for file in dict.fromkeys([*files, *([startup_log] if startup_log else [])]):
            snapshot = rollout_snapshot.read_rollout_snapshot(file)
            stack.callback(snapshot.stream.close)
            captured[file] = snapshot
            captured_bytes += snapshot.byte_length
            if captured_bytes > MAX_CAPTURE_BYTES:
                break
            if file not in files:
                continue
            retained_lines = [] if decoded_wire_bytes <= MAX_DECODED_WIRE_BYTES else None
            # The miss path consumes these exact parsed values rather than
            # decoding/decompressing the captured JSONL again. Keep errors too.
            with contextlib.closing(snapshot.decoded_lines()) as lines:
                for number, item, error, wire_bytes in lines:
                    if retained_lines is not None:
                        decoded_records += 1
                        decoded_wire_bytes += wire_bytes
                        if (decoded_records <= MAX_DECODED_RECORDS
                                and decoded_wire_bytes <= MAX_DECODED_WIRE_BYTES):
                            retained_lines.append((number, item, error, wire_bytes))
                        else:
                            retained_lines = None
                    if error is not None:
                        continue
                    if (
                        not isinstance(item, dict)
                        or item.get("type") != "rollout_payload_artifact"
                    ):
                        continue
                    ref = item.get("payload")
                    if not isinstance(ref, dict) or type(ref.get("bytes")) is not int:
                        raise ValueError("invalid rollout payload reference")
                    key = (
                        rollout_snapshot.rollout_payload_root(snapshot.path),
                        ref.get("sha256"),
                    )
                    if key not in payloads:
                        data = rollout_snapshot.load_rollout_payload(
                            snapshot.path, ref.get("sha256"), ref["bytes"]
                        )
                        payloads[key] = data
                        payload_ids.append(
                            {
                                "root": str(
                                    rollout_snapshot.rollout_payload_root(snapshot.path)
                                ),
                                "sha256": ref["sha256"],
                                "bytes": len(data),
                            }
                        )
                        captured_bytes += len(data)
                    if len(payloads[key]) != ref["bytes"]:
                        raise ValueError("rollout payload size mismatch")
                    if captured_bytes > MAX_CAPTURE_BYTES:
                        break
            if retained_lines is not None and captured_bytes <= MAX_CAPTURE_BYTES:
                decoded[file] = retained_lines
            if captured_bytes > MAX_CAPTURE_BYTES:
                break

        def hydrate(item, path):
            if (
                not isinstance(item, dict)
                or item.get("type") != "rollout_payload_artifact"
            ):
                return item
            ref = item.get("payload")
            if not isinstance(ref, dict) or type(ref.get("bytes")) is not int:
                raise ValueError("invalid rollout payload reference")
            digest = ref.get("sha256")
            data = payloads.get((rollout_snapshot.rollout_payload_root(path), digest)) if isinstance(digest, str) else None
            if data is None:
                return rollout_snapshot.hydrate_rollout_record(item, path)
            if len(data) != ref["bytes"]:
                raise ValueError("rollout payload size mismatch")
            return rollout_snapshot._hydrate_verified_payload(item, data)

        if captured_bytes > MAX_CAPTURE_BYTES:
            # Bypass persistence, not evidence already acquired in this call.
            # The analyzer owns only the remaining snapshots; this stack closes
            # the captured prefix. Never expose a partially decoded file as EOF
            # or extend the bounded payload pool while reading the remainder.
            report = analyze(
                source, repo_root, **options, _captured=captured, _files=files,
                _hydrate=hydrate, _decoded=decoded,
            )
            report["analysisCache"] = {
                "status": "bypassed",
                "reason": "capture budget exceeded",
            }
            return report
        implementation = analyzer_identity()
        provenance = {
            "version": 1,
            "analyzer": implementation,
            "runtime": [sys.version, sys.platform, platform.machine()],
            "source": str(source.resolve()) if source else None,
            "source_argument": str(source) if source else None,
            "repo_root": str(repo_root.resolve()),
            "repo_root_argument": str(repo_root),
            "files": [str(file) for file in files],
            "snapshots": [snapshot.metadata() for snapshot in captured.values()],
            "payloads": payload_ids,
            "tokens": include_tokens,
            "runner_evidence": runner_evidence,
            "diagnostic_evidence": diagnostic_evidence,
            "startup_log": str(startup_log.resolve()) if startup_log else None,
        }
        identity = _sha(_bytes(provenance))
        retained = None if refresh else _read_entry(root, identity)
        if retained is not None:
            report, path, report_bytes = retained
            report["analysisCache"] = {
                "status": "hit",
                "input_sha256": identity,
                "report": str(path),
                "report_sha256": path.stem,
                "report_bytes": report_bytes,
                "freshness": "captured inputs only",
            }
            return report

        report = analyze(
            source,
            repo_root,
            **options,
            _captured=captured,
            _files=files,
            _hydrate=hydrate,
            _decoded=decoded,
        )
        report["inputProvenance"] = {"sha256": identity, "inputs": provenance}
        # Existing command-output lineage consumes this declaration as evidence
        # attribution only, never freshness, completion, or validation authority.
        # Native file reads still authenticate the content-addressed report bytes.
        report["evidence_lineage"] = {
            "source": "rollout-audit",
            "scope": {
                "source": provenance["source"],
                "repo_root": provenance["repo_root"],
            },
            "identity": {"input_sha256": identity},
        }
        if analyzer_identity() != implementation:
            report["analysisCache"] = {
                "status": "bypassed",
                "reason": "analyzer changed during capture",
            }
            return report
        data = _bytes(report)
        path = root / "reports" / f"{_sha(data)}.json"
        if len(data) > MAX_REPORT_BYTES:
            report["analysisCache"] = {
                "status": "bypassed",
                "reason": "report budget exceeded",
            }
            return report
        try:
            write_bytes_atomic(path, data, immutable=True)
            write_json_atomic(
                root / "inputs" / f"{identity}.json",
                {"input_sha256": identity, "report_sha256": _sha(data)},
            )
            report["analysisCache"] = {
                "status": "miss",
                "input_sha256": identity,
                "report": str(path),
                "report_sha256": path.stem,
                "report_bytes": len(data),
                "freshness": "captured inputs only",
            }
        except (OSError, ValueError) as error:
            # Optional persistence must not discard a valid freshly computed report.
            report["analysisCache"] = {
                "status": "unavailable",
                "reason": type(error).__name__,
            }
        return report
