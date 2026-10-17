"""Harbor interpreter lookup retained for maintenance; benchmarks retired."""
from pathlib import Path
import subprocess


def harbor_python(explicit=None):
    if explicit:
        return Path(explicit).resolve(strict=True)
    result = subprocess.run(["uv", "tool", "dir"], capture_output=True, text=True, check=True)
    path = Path(result.stdout.strip()) / "harbor" / "Scripts" / "python.exe"
    if not path.is_file():
        raise ValueError("Install harbor[modal]==0.24.0 or pass --harbor-python")
    return path


if __name__ == "__main__":
    raise SystemExit("Live benchmark launcher retired; use KDR for saved-session audits.")
