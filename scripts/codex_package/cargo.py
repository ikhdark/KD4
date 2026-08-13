"""Cargo builds for source-built Codex package artifacts."""

import hashlib
import json
import os
import shutil
import subprocess
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

from .targets import REPO_ROOT
from .targets import PackageVariant
from .targets import TargetSpec
from .v8 import resolve_codex_v8_cargo_env


CODEX_RS_ROOT = REPO_ROOT / "codex-rs"
DEFAULT_RUST_MIN_STACK = "8388608"
# One shared local default with scripts/just-shell.py and common-rust-env.ps1;
# override everywhere with CODEX_SCCACHE_CACHE_SIZE.
SCCACHE_CACHE_SIZE_ENV_VAR = "CODEX_SCCACHE_CACHE_SIZE"
DEFAULT_SCCACHE_CACHE_SIZE = "80G"
PACKAGE_TARGET_DIR_ENV = "CODEX_PACKAGE_TARGET_DIR"
SOURCE_BUILD_STAMP = "codex-package-source-builds.json"
WINDOWS_LLVM_LLD_LINK_DEFAULT = Path("C:/Program Files/LLVM/bin/lld-link.exe")
SCOOP_LLVM_LLD_LINK = Path("apps/llvm/current/bin/lld-link.exe")


@dataclass(frozen=True)
class SourceBuildOutputs:
    entrypoint_bin: Path
    code_mode_host_bin: Path
    bwrap_bin: Path | None
    codex_command_runner_bin: Path | None
    codex_windows_sandbox_setup_bin: Path | None


def build_source_binaries(
    spec: TargetSpec,
    variant: PackageVariant,
    *,
    cargo: str,
    profile: str,
    entrypoint_bin: Path | None,
    code_mode_host_bin: Path | None,
    bwrap_bin: Path | None,
    codex_command_runner_bin: Path | None,
    codex_windows_sandbox_setup_bin: Path | None,
    reuse_existing: bool = False,
    force_rebuild: bool = False,
) -> SourceBuildOutputs:
    validate_prebuilt_resource_inputs(
        spec,
        bwrap_bin=bwrap_bin,
        codex_command_runner_bin=codex_command_runner_bin,
        codex_windows_sandbox_setup_bin=codex_windows_sandbox_setup_bin,
    )
    validate_explicit_output_paths(
        entrypoint_bin=entrypoint_bin,
        code_mode_host_bin=code_mode_host_bin,
        bwrap_bin=bwrap_bin,
        codex_command_runner_bin=codex_command_runner_bin,
        codex_windows_sandbox_setup_bin=codex_windows_sandbox_setup_bin,
    )

    target_dir = cargo_package_target_dir(spec, profile)
    output_dir = cargo_profile_output_dir(spec, profile, target_dir=target_dir)
    outputs = SourceBuildOutputs(
        entrypoint_bin=resolve_output_path(
            entrypoint_bin,
            output_dir / variant.entrypoint_name(spec),
        ),
        code_mode_host_bin=resolve_output_path(
            code_mode_host_bin,
            output_dir / spec.code_mode_host_name,
        ),
        bwrap_bin=resolve_output_path(
            bwrap_bin,
            output_dir / "bwrap" if spec.is_linux else None,
        ),
        codex_command_runner_bin=resolve_output_path(
            codex_command_runner_bin,
            output_dir / "codex-command-runner.exe" if spec.is_windows else None,
        ),
        codex_windows_sandbox_setup_bin=resolve_output_path(
            codex_windows_sandbox_setup_bin,
            output_dir / "codex-windows-sandbox-setup.exe" if spec.is_windows else None,
        ),
    )

    requested_binaries = source_binaries_for_target(
        spec,
        variant,
        build_entrypoint=entrypoint_bin is None,
        build_code_mode_host=code_mode_host_bin is None,
        build_bwrap=spec.is_linux and bwrap_bin is None,
        build_codex_command_runner=spec.is_windows and codex_command_runner_bin is None,
        build_codex_windows_sandbox_setup=spec.is_windows
        and codex_windows_sandbox_setup_bin is None,
    )
    binaries = binaries_missing_for_reuse(
        requested_binaries,
        outputs=outputs,
        variant=variant,
        target_dir=target_dir,
        spec=spec,
        profile=profile,
        reuse_existing=reuse_existing,
        force_rebuild=force_rebuild,
        cargo=cargo,
    )
    if requested_binaries and not binaries:
        print(
            "package cargo reuse: "
            f"bins={','.join(requested_binaries)} target_dir={target_dir}"
        )

    if binaries:
        run_cargo_build(
            cargo,
            spec,
            profile,
            binaries,
            target_dir=target_dir,
            include_v8_env=(
                variant.cargo_bin in binaries or "codex-code-mode-host" in binaries
            ),
        )

    validate_source_outputs(outputs)
    if binaries:
        write_source_build_stamp(
            target_dir,
            spec=spec,
            profile=profile,
            variant=variant,
            outputs=outputs,
            cargo=cargo,
        )
    return outputs


def run_cargo_build(
    cargo: str,
    spec: TargetSpec,
    profile: str,
    binaries: list[str],
    *,
    target_dir: Path,
    include_v8_env: bool,
) -> None:
    cmd = [
        cargo,
        "build",
        "--locked",
        "--target-dir",
        str(target_dir),
        "--target",
        spec.target,
        "--profile",
        profile,
    ]
    for binary in binaries:
        cmd.extend(["--bin", binary])

    cargo_env = cargo_build_env(spec, profile, target_dir=target_dir)
    if include_v8_env:
        codex_v8_env = resolve_codex_v8_cargo_env(spec)
        if codex_v8_env:
            cargo_env.update(codex_v8_env)

    print("+", " ".join(cmd))
    start = time.perf_counter()
    try:
        subprocess.run(
            cmd,
            cwd=CODEX_RS_ROOT,
            check=True,
            env=cargo_env,
        )
    except subprocess.CalledProcessError as exc:
        raise RuntimeError(
            "package cargo build failed: "
            f"bins={','.join(binaries)} "
            f"target={spec.target} "
            f"profile={profile} "
            f"target_dir={target_dir} "
            f"exit_code={exc.returncode}"
        ) from exc
    elapsed = time.perf_counter() - start
    print(
        "package cargo build: "
        f"bins={','.join(binaries)} "
        f"target_dir={target_dir} "
        f"profile={profile} "
        f"elapsed={elapsed:.2f}s"
    )


def source_binaries_for_target(
    spec: TargetSpec,
    variant: PackageVariant,
    *,
    build_entrypoint: bool,
    build_code_mode_host: bool,
    build_bwrap: bool,
    build_codex_command_runner: bool,
    build_codex_windows_sandbox_setup: bool,
) -> list[str]:
    binaries = []
    if build_entrypoint:
        binaries.append(variant.cargo_bin)
    if build_code_mode_host:
        binaries.append("codex-code-mode-host")
    if build_bwrap:
        binaries.append("bwrap")
    if build_codex_command_runner:
        binaries.append("codex-command-runner")
    if build_codex_windows_sandbox_setup:
        binaries.append("codex-windows-sandbox-setup")
    return binaries


def validate_prebuilt_resource_inputs(
    spec: TargetSpec,
    *,
    bwrap_bin: Path | None,
    codex_command_runner_bin: Path | None,
    codex_windows_sandbox_setup_bin: Path | None,
) -> None:
    if bwrap_bin is not None and not spec.is_linux:
        raise RuntimeError("--bwrap-bin is only supported for Linux targets.")
    if codex_command_runner_bin is not None and not spec.is_windows:
        raise RuntimeError(
            "--codex-command-runner-bin is only supported for Windows targets."
        )
    if codex_windows_sandbox_setup_bin is not None and not spec.is_windows:
        raise RuntimeError(
            "--codex-windows-sandbox-setup-bin is only supported for Windows targets."
        )


def validate_explicit_output_paths(
    *,
    entrypoint_bin: Path | None,
    code_mode_host_bin: Path | None,
    bwrap_bin: Path | None,
    codex_command_runner_bin: Path | None,
    codex_windows_sandbox_setup_bin: Path | None,
) -> None:
    explicit_paths = [
        ("--entrypoint-bin", "prebuilt entrypoint executable", entrypoint_bin),
        ("--code-mode-host-bin", "prebuilt code-mode host executable", code_mode_host_bin),
        ("--bwrap-bin", "prebuilt Linux bwrap executable", bwrap_bin),
        (
            "--codex-command-runner-bin",
            "prebuilt Windows codex-command-runner.exe executable",
            codex_command_runner_bin,
        ),
        (
            "--codex-windows-sandbox-setup-bin",
            "prebuilt Windows codex-windows-sandbox-setup.exe executable",
            codex_windows_sandbox_setup_bin,
        ),
    ]
    resolved: list[tuple[str, Path]] = []
    for flag, description, path in explicit_paths:
        if path is not None and not path.is_file():
            raise RuntimeError(f"{description} does not exist: {path}")
        if path is None:
            continue
        canonical = path.resolve(strict=True)
        for prior_flag, prior_path in resolved:
            if canonical == prior_path or canonical.samefile(prior_path):
                raise RuntimeError(
                    f"{flag} and {prior_flag} must refer to distinct executables; "
                    f"both resolve to {canonical}"
                )
        resolved.append((flag, canonical))


def resolve_output_path(
    explicit_path: Path | None, default_path: Path | None
) -> Path | None:
    if explicit_path is not None:
        return explicit_path.resolve()

    return default_path


def cargo_profile_output_dir(
    spec: TargetSpec,
    profile: str,
    *,
    target_dir: Path | None = None,
) -> Path:
    target_dir = cargo_target_dir() if target_dir is None else target_dir
    return target_dir / spec.target / cargo_profile_dirname(profile)


def cargo_package_target_dir(spec: TargetSpec, profile: str) -> Path:
    package_target_dir = os.environ.get(PACKAGE_TARGET_DIR_ENV)
    if package_target_dir is not None:
        return resolve_cargo_target_dir(package_target_dir)

    return (
        CODEX_RS_ROOT
        / "target"
        / "package"
        / f"{spec.target}-{cargo_profile_dirname(profile)}"
    )


def cargo_target_dir() -> Path:
    target_dir = os.environ.get("CARGO_TARGET_DIR")
    if target_dir is None:
        return CODEX_RS_ROOT / "target"

    return resolve_cargo_target_dir(target_dir)


def resolve_cargo_target_dir(target_dir: str) -> Path:
    # Cargo resolves relative CARGO_TARGET_DIR values from its working directory.
    # run_cargo_build uses cwd=CODEX_RS_ROOT, so keep this helper tied to that cwd.
    path = Path(target_dir)
    if path.is_absolute():
        return path

    return CODEX_RS_ROOT / path


def cargo_build_env(
    spec: TargetSpec,
    profile: str,
    *,
    target_dir: Path,
) -> dict[str, str]:
    env = dict(os.environ)
    env.pop("CARGO_TARGET_DIR", None)
    env.setdefault("RUST_MIN_STACK", DEFAULT_RUST_MIN_STACK)
    static_msvc_flags = static_msvc_rustflags(spec, profile)
    if static_msvc_flags:
        env_name = cargo_target_rustflags_env_name(spec.target)
        existing = env.get(env_name, "").strip()
        flags = " ".join(static_msvc_flags)
        env[env_name] = f"{existing} {flags}".strip()
    if spec.is_windows and spec.target.endswith("-msvc"):
        linker_env_name = cargo_target_linker_env_name(spec.target)
        if not env.get(linker_env_name):
            lld_link = find_windows_lld_link()
            if lld_link:
                env[linker_env_name] = lld_link
    rustc_wrapper = env.get("RUSTC_WRAPPER")
    if not rustc_wrapper and shutil.which("sccache"):
        env["RUSTC_WRAPPER"] = "sccache"
        set_sccache_env(env)
    elif rustc_wrapper and is_sccache_wrapper(rustc_wrapper):
        set_sccache_env(env)
    return env


def set_sccache_env(env: dict[str, str]) -> None:
    env["SCCACHE_BASEDIR"] = str(REPO_ROOT.resolve())
    override = (env.get(SCCACHE_CACHE_SIZE_ENV_VAR) or "").strip()
    env["SCCACHE_CACHE_SIZE"] = override or DEFAULT_SCCACHE_CACHE_SIZE


def is_sccache_wrapper(value: str) -> bool:
    leaf = Path(value).name.lower()
    return value.lower() in {"sccache", "sccache.exe"} or leaf in {
        "sccache",
        "sccache.exe",
    }


def cargo_target_rustflags_env_name(target: str) -> str:
    return f"CARGO_TARGET_{target.upper().replace('-', '_')}_RUSTFLAGS"


def cargo_target_linker_env_name(target: str) -> str:
    return f"CARGO_TARGET_{target.upper().replace('-', '_')}_LINKER"


def find_windows_lld_link() -> str | None:
    lld_link = shutil.which("lld-link")
    if lld_link:
        return lld_link
    if WINDOWS_LLVM_LLD_LINK_DEFAULT.exists():
        return str(WINDOWS_LLVM_LLD_LINK_DEFAULT)
    for root in scoop_roots():
        candidate = root / SCOOP_LLVM_LLD_LINK
        if candidate.exists():
            return str(candidate)
    return None


def scoop_roots() -> tuple[Path, ...]:
    roots: list[Path] = []
    for raw in (os.environ.get("SCOOP"), os.environ.get("USERPROFILE")):
        if not raw:
            continue
        root = Path(raw)
        if raw == os.environ.get("USERPROFILE"):
            root = root / "scoop"
        if root not in roots:
            roots.append(root)
    return tuple(roots)


def static_msvc_rustflags(spec: TargetSpec, profile: str) -> tuple[str, ...]:
    if not spec.is_windows or not spec.target.endswith("-msvc") or profile == "dev":
        return ()

    flags = ["-C", "link-arg=/STACK:8388608", "-C", "target-feature=+crt-static"]
    if spec.target == "aarch64-pc-windows-msvc":
        flags.extend(["-C", "link-arg=/arm64hazardfree"])
    return tuple(flags)


def cargo_profile_dirname(profile: str) -> str:
    if profile == "dev":
        return "debug"
    if profile == "release":
        return "release"
    return profile


def binaries_missing_for_reuse(
    binaries: list[str],
    *,
    outputs: SourceBuildOutputs,
    variant: PackageVariant,
    target_dir: Path,
    spec: TargetSpec,
    profile: str,
    reuse_existing: bool,
    force_rebuild: bool,
    cargo: str = "cargo",
) -> list[str]:
    if force_rebuild or not reuse_existing:
        return binaries

    stamp = read_source_build_stamp(target_dir)
    if stamp is None or not source_build_stamp_metadata_matches(
        stamp,
        spec=spec,
        profile=profile,
        variant=variant,
        cargo=cargo,
    ):
        return binaries

    stamp_outputs = stamp.get("outputs")
    if not isinstance(stamp_outputs, dict):
        return binaries

    missing = []
    for binary in binaries:
        output_key = source_output_key_for_binary(binary, variant=variant)
        output = expected_output_for_binary(
            binary,
            outputs=outputs,
            variant=variant,
        )
        if not source_output_matches_fingerprint(output, stamp_outputs.get(output_key)):
            missing.append(binary)

    if missing == binaries:
        return binaries
    if not source_build_stamp_source_matches(stamp):
        return binaries
    return missing


def expected_output_for_binary(
    binary: str,
    *,
    outputs: SourceBuildOutputs,
    variant: PackageVariant,
) -> Path:
    if binary == variant.cargo_bin:
        return outputs.entrypoint_bin
    if binary == "codex-code-mode-host":
        return outputs.code_mode_host_bin
    if binary == "bwrap" and outputs.bwrap_bin is not None:
        return outputs.bwrap_bin
    if (
        binary == "codex-command-runner"
        and outputs.codex_command_runner_bin is not None
    ):
        return outputs.codex_command_runner_bin
    if (
        binary == "codex-windows-sandbox-setup"
        and outputs.codex_windows_sandbox_setup_bin is not None
    ):
        return outputs.codex_windows_sandbox_setup_bin
    raise RuntimeError(f"unknown source binary output: {binary}")


def source_output_key_for_binary(
    binary: str,
    *,
    variant: PackageVariant,
) -> str:
    if binary == variant.cargo_bin:
        return "entrypoint_bin"
    if binary == "codex-code-mode-host":
        return "code_mode_host_bin"
    if binary == "bwrap":
        return "bwrap_bin"
    if binary == "codex-command-runner":
        return "codex_command_runner_bin"
    if binary == "codex-windows-sandbox-setup":
        return "codex_windows_sandbox_setup_bin"
    raise RuntimeError(f"unknown source binary output: {binary}")


def validate_source_outputs(outputs: SourceBuildOutputs) -> None:
    for path in [
        outputs.entrypoint_bin,
        outputs.code_mode_host_bin,
        outputs.bwrap_bin,
        outputs.codex_command_runner_bin,
        outputs.codex_windows_sandbox_setup_bin,
    ]:
        if path is not None and not path.is_file():
            raise RuntimeError(f"cargo build did not produce expected binary: {path}")


def source_build_stamp_path(target_dir: Path) -> Path:
    return target_dir / SOURCE_BUILD_STAMP


def read_source_build_stamp(target_dir: Path) -> dict | None:
    path = source_build_stamp_path(target_dir)
    if not path.is_file():
        return None
    try:
        stamp = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    return stamp if isinstance(stamp, dict) else None


def write_source_build_stamp(
    target_dir: Path,
    *,
    spec: TargetSpec,
    profile: str,
    variant: PackageVariant,
    outputs: SourceBuildOutputs,
    cargo: str = "cargo",
) -> None:
    stamp = {
        "target": spec.target,
        "profile": profile,
        "variant": variant.name,
        "build_recipe": build_recipe_fingerprint(
            spec=spec, profile=profile, cargo=cargo
        ),
        "source": source_tree_fingerprint(),
        "outputs": source_output_fingerprints(outputs),
    }
    path = source_build_stamp_path(target_dir)
    path.parent.mkdir(parents=True, exist_ok=True)
    contents = json.dumps(stamp, sort_keys=True, indent=2) + "\n"
    file_descriptor, temporary_name = tempfile.mkstemp(
        dir=path.parent,
        prefix=f".{path.name}.",
        suffix=".tmp",
    )
    temporary_path = Path(temporary_name)
    try:
        with os.fdopen(file_descriptor, "w", encoding="utf-8", newline="\n") as file:
            file.write(contents)
            file.flush()
            os.fsync(file.fileno())
        os.replace(temporary_path, path)
    except BaseException:
        temporary_path.unlink(missing_ok=True)
        raise


def source_build_stamp_matches(
    target_dir: Path,
    *,
    spec: TargetSpec,
    profile: str,
    variant: PackageVariant,
    outputs: SourceBuildOutputs,
    cargo: str = "cargo",
) -> bool:
    stamp = read_source_build_stamp(target_dir)
    if stamp is None:
        return False

    if not source_build_stamp_metadata_matches(
        stamp,
        spec=spec,
        profile=profile,
        variant=variant,
        cargo=cargo,
    ):
        return False

    if not source_outputs_match_fingerprints(outputs, stamp.get("outputs")):
        return False

    return source_build_stamp_source_matches(stamp)


def source_build_stamp_metadata_matches(
    stamp: dict,
    *,
    spec: TargetSpec,
    profile: str,
    variant: PackageVariant,
    cargo: str = "cargo",
) -> bool:
    return (
        stamp.get("target") == spec.target
        and stamp.get("profile") == profile
        and stamp.get("variant") == variant.name
        and stamp.get("build_recipe")
        == build_recipe_fingerprint(spec=spec, profile=profile, cargo=cargo)
    )


def build_recipe_fingerprint(
    *, spec: TargetSpec, profile: str, cargo: str = "cargo"
) -> dict[str, object]:
    """Capture toolchain and environment inputs that can change Cargo output."""
    rustc = os.environ.get("RUSTC", "rustc")
    environment_names = {
        name
        for name in os.environ
        if name.startswith("CARGO_PROFILE_")
        or name.startswith("CARGO_TARGET_")
        or "V8" in name
    }
    environment_names.update(
        {
            "AR",
            "CARGO",
            "CARGO_ENCODED_RUSTFLAGS",
            "CC",
            "CXX",
            "RUSTC",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
            "RUSTFLAGS",
        }
    )
    environment: dict[str, object] = {}
    for name in sorted(environment_names):
        value = os.environ.get(name)
        if value is None:
            continue
        candidate = Path(value).expanduser()
        environment[name] = (
            source_output_fingerprint(candidate) if candidate.is_file() else value
        )

    return {
        "target": spec.target,
        "profile": profile,
        "cargo": command_identity(cargo, "--version", "--verbose"),
        "rustc": command_identity(rustc, "-Vv"),
        "environment": environment,
        "profile_config": files_fingerprint(
            (
                REPO_ROOT / "Cargo.toml",
                REPO_ROOT / "codex-rs" / "Cargo.toml",
                REPO_ROOT / ".cargo" / "config",
                REPO_ROOT / ".cargo" / "config.toml",
                REPO_ROOT / "codex-rs" / ".cargo" / "config",
                REPO_ROOT / "codex-rs" / ".cargo" / "config.toml",
            )
        ),
    }


def command_identity(command: str, *args: str) -> dict[str, object]:
    executable = shutil.which(command) or command
    try:
        completed = subprocess.run(
            [executable, *args],
            check=True,
            cwd=REPO_ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
    except (OSError, subprocess.CalledProcessError) as error:
        return {"path": executable, "status": "unavailable", "error": str(error)}
    return {
        "path": str(Path(executable).resolve()),
        "version": completed.stdout.strip(),
    }


def files_fingerprint(paths: tuple[Path, ...]) -> dict[str, object]:
    return {
        str(path.relative_to(REPO_ROOT)): source_output_fingerprint(path)
        for path in paths
        if path.is_file()
    }


def source_build_stamp_source_matches(stamp: dict) -> bool:
    source = source_tree_fingerprint()
    return source.get("status") == "ok" and stamp.get("source") == source


def source_tree_fingerprint() -> dict[str, str]:
    git = shutil.which("git")
    if git is None:
        return {"status": "unavailable", "reason": "git-not-found"}

    try:
        head = (
            run_git_bytes(git, "rev-parse", "HEAD")
            .decode("utf-8", "surrogateescape")
            .strip()
        )
        index_tree = (
            run_git_bytes(git, "write-tree").decode("utf-8", "surrogateescape").strip()
        )
        tracked_diff = run_git_bytes(git, "diff", "--binary", "HEAD", "--", ".")
        untracked_names = run_git_bytes(
            git, "ls-files", "--others", "--exclude-standard", "-z", "--", "."
        )
        # Hash untracked file CONTENTS too: name-only hashing lets an edit to
        # a not-yet-added source file reuse stale binaries silently.
        untracked_contents = hashlib.sha256()
        for name in untracked_names.split(b"\0"):
            if not name:
                continue
            untracked_contents.update(name)
            untracked_contents.update(b"\0")
            file_path = CODEX_RS_ROOT / name.decode("utf-8", "surrogateescape")
            try:
                untracked_contents.update(file_path.read_bytes())
            except OSError:
                untracked_contents.update(b"<unreadable>")
            untracked_contents.update(b"\0")
    except (OSError, subprocess.CalledProcessError):
        return {"status": "unavailable", "reason": "git-unavailable"}

    return {
        "status": "ok",
        "git_head": head,
        "index_tree": index_tree,
        "working_tree_sha256": hashlib.sha256(tracked_diff).hexdigest(),
        "untracked_names_sha256": hashlib.sha256(untracked_names).hexdigest(),
        "untracked_contents_sha256": untracked_contents.hexdigest(),
    }


def run_git_bytes(git: str, *args: str) -> bytes:
    process = subprocess.Popen(
        [git, *args],
        cwd=CODEX_RS_ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    stdout, _ = process.communicate()
    if process.returncode != 0:
        raise subprocess.CalledProcessError(process.returncode, [git, *args])
    return stdout


def source_output_fingerprints(outputs: SourceBuildOutputs) -> dict[str, dict | None]:
    return {
        "entrypoint_bin": source_output_fingerprint(outputs.entrypoint_bin),
        "code_mode_host_bin": source_output_fingerprint(outputs.code_mode_host_bin),
        "bwrap_bin": source_output_fingerprint(outputs.bwrap_bin),
        "codex_command_runner_bin": source_output_fingerprint(
            outputs.codex_command_runner_bin
        ),
        "codex_windows_sandbox_setup_bin": source_output_fingerprint(
            outputs.codex_windows_sandbox_setup_bin
        ),
    }


def source_output_fingerprint(path: Path | None) -> dict | None:
    if path is None or not path.is_file():
        return None
    stat = path.stat()
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(chunk)
    return {
        "path": str(path),
        "size": stat.st_size,
        "mtime_ns": stat.st_mtime_ns,
        "sha256": digest.hexdigest(),
    }


def source_outputs_match_fingerprints(
    outputs: SourceBuildOutputs, fingerprints: object
) -> bool:
    if not isinstance(fingerprints, dict):
        return False
    return all(
        source_output_matches_fingerprint(path, fingerprints.get(key))
        for key, path in {
            "entrypoint_bin": outputs.entrypoint_bin,
            "code_mode_host_bin": outputs.code_mode_host_bin,
            "bwrap_bin": outputs.bwrap_bin,
            "codex_command_runner_bin": outputs.codex_command_runner_bin,
            "codex_windows_sandbox_setup_bin": outputs.codex_windows_sandbox_setup_bin,
        }.items()
    )


def source_output_matches_fingerprint(path: Path | None, fingerprint: object) -> bool:
    if path is None:
        return fingerprint is None
    if not isinstance(fingerprint, dict) or not path.is_file():
        return False
    return fingerprint == source_output_fingerprint(path)
