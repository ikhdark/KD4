"""Source-inventory regression helpers; standalone benchmarking retired."""
import hashlib
import shutil
import subprocess
from scripts.maintenance import source_inventory as inventory


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def capture(root, scope, target):
    sources = inventory.repository_source_records(root, prune=inventory.PRUNE, paths=(scope,))
    target.mkdir()
    subprocess.run(["git", "init", "-q", str(target)], check=True, capture_output=True)
    # Only the disposable captured repository needs this Windows path policy.
    subprocess.run(["git", "-C", str(target), "config", "--local", "core.longpaths", "true"],
                   check=True, capture_output=True)
    manifest = []
    tracked = []
    for name, tracking in sorted(sources.items()):
        source = root / name
        if tracking == "deleted" or source.is_symlink() or not source.resolve().is_relative_to(root):
            raise ValueError(f"snapshot requires a regular available source: {name}")
        before = inventory.source_revision(source)
        destination = target / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, destination)
        if inventory.source_revision(source) != before:
            raise ValueError(f"source changed while capturing: {name}")
        manifest.append({"path": name, "tracking": tracking, "sha256": sha256(destination)})
        if tracking == "tracked":
            tracked.append(name.encode("utf-8") + b"\0")
    if tracked:
        subprocess.run(
            ["git", "-c", "core.autocrlf=false", "add", "-f",
             "--pathspec-from-file=-", "--pathspec-file-nul"],
            cwd=target, input=b"".join(tracked), capture_output=True, check=True,
        )
    return manifest


def correctness_checks(actual, expected):
    """Expected artifacts are explicit proof obligations, not inferred truth."""
    return {
        key: key in actual and key in expected and actual[key] == expected[key]
        for key in ("query_id", "source_snapshot_sha256", "paths", "categories",
                    "untracked_paths", "unresolved", "missing_categories")
    }


if __name__ == "__main__":
    raise SystemExit("Harness benchmark retired; use KDR for saved-session audits.")
