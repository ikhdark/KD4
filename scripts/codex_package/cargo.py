"""Cargo builds for source-built Codex package artifacts."""

import hashlib
from contextlib import contextmanager
from contextvars import ContextVar
from functools import wraps
import json
import os
import re
import shutil
import subprocess
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

try:
    from scripts import rust_tool_env as shared_rust_tool_env
    from scripts.generated_output_lock import repository_lock
except ImportError:
    import rust_tool_env as shared_rust_tool_env
    from generated_output_lock import repository_lock

from scripts.process_owner import run_owned
from .layout import pe_machine
from .targets import REPO_ROOT
from .targets import PackageVariant
from .targets import TargetSpec


CODEX_RS_ROOT = REPO_ROOT / "codex-rs"
DEFAULT_RUST_MIN_STACK = "8388608"
# One shared local default with scripts/just-shell.py and common-rust-env.ps1;
# override everywhere with CODEX_SCCACHE_CACHE_SIZE.
SCCACHE_CACHE_SIZE_ENV_VAR = shared_rust_tool_env.SCCACHE_CACHE_SIZE_ENV_VAR
DEFAULT_SCCACHE_CACHE_SIZE = shared_rust_tool_env.DEFAULT_SCCACHE_CACHE_SIZE
PACKAGE_TARGET_DIR_ENV = "CODEX_PACKAGE_TARGET_DIR"
SOURCE_BUILD_STAMP = "codex-package-source-builds.json"
DISCOVERY_TIMEOUT_SECONDS = 15
WINDOWS_LLVM_LLD_LINK_DEFAULT = shared_rust_tool_env.WINDOWS_LLVM_LLD_LINK_DEFAULT


@dataclass(frozen=True)
class SourceBuildOutputs:
    entrypoint_bin: Path
    code_mode_host_bin: Path
    codex_command_runner_bin: Path | None
    codex_windows_sandbox_setup_bin: Path | None


_build_leases = ContextVar("package_build_leases", default=frozenset())
_leased_targets = ContextVar("package_lease_targets", default=None)


@contextmanager
def package_build_lease(spec, profile, *, inputs=None):
    if inputs is not None and all(
        path is not None
        and not (path.is_file() and path.stat().st_nlink > 1)
        and not path.resolve().is_relative_to(
            package_target_base(spec, profile).resolve()
        )
        for path in inputs
    ):
        yield None
        return
    target = cargo_package_target_dir(spec, profile).resolve()
    held = _build_leases.get()
    if target in held:
        yield target
        return
    with repository_lock(
        target.with_name(target.name + ".build.lock"),
        "package builder",
        "package build target",
    ):
        token = _build_leases.set(held | {target})
        targets_token = _leased_targets.set({
            **(_leased_targets.get() or {}),
            (spec.target, profile, str(package_target_base(spec, profile).resolve())): target,
        })
        try:
            yield target
        finally:
            _leased_targets.reset(targets_token)
            _build_leases.reset(token)


def _leased_build(function):
    @wraps(function)
    def wrapped(spec, variant, **kwargs):
        with package_build_lease(
            spec,
            kwargs["profile"],
            inputs=tuple(
                kwargs.get(name) for name in SourceBuildOutputs.__dataclass_fields__
            ),
        ):
            return function(spec, variant, **kwargs)

    return wrapped


@_leased_build
def build_source_binaries(
    spec: TargetSpec,
    variant: PackageVariant,
    *,
    cargo: str,
    profile: str,
    entrypoint_bin: Path | None,
    code_mode_host_bin: Path | None,
    codex_command_runner_bin: Path | None,
    codex_windows_sandbox_setup_bin: Path | None,
    reuse_existing: bool = False,
    force_rebuild: bool = False,
    release_version: str | None = None,
) -> SourceBuildOutputs:
    validate_prebuilt_executable_targets(
        spec,
        entrypoint_bin=entrypoint_bin,
        code_mode_host_bin=code_mode_host_bin,
        codex_command_runner_bin=codex_command_runner_bin,
        codex_windows_sandbox_setup_bin=codex_windows_sandbox_setup_bin,
    )
    validate_explicit_output_paths(
        entrypoint_bin=entrypoint_bin,
        code_mode_host_bin=code_mode_host_bin,
        codex_command_runner_bin=codex_command_runner_bin,
        codex_windows_sandbox_setup_bin=codex_windows_sandbox_setup_bin,
    )

    requested_binaries = source_binaries_for_target(
        spec,
        variant,
        build_entrypoint=entrypoint_bin is None,
        build_code_mode_host=code_mode_host_bin is None,
        build_codex_command_runner=codex_command_runner_bin is None,
        build_codex_windows_sandbox_setup=codex_windows_sandbox_setup_bin is None,
    )
    target_dir = (
        cargo_package_target_dir(spec, profile)
        if requested_binaries else package_target_base(spec, profile)
    )
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
        codex_command_runner_bin=resolve_output_path(
            codex_command_runner_bin,
            output_dir / "codex-command-runner.exe",
        ),
        codex_windows_sandbox_setup_bin=resolve_output_path(
            codex_windows_sandbox_setup_bin,
            output_dir / "codex-windows-sandbox-setup.exe",
        ),
    )
    validate_distinct_output_paths(outputs)

    build_env = (
        cargo_build_env(
            spec, profile, target_dir=target_dir, release_version=release_version
        )
        if requested_binaries
        else None
    )
    if build_env is not None:
        reject_rustflags_overrides(build_env)
    # Evidence belongs to this invocation only. Reuse discovers it lazily;
    # a build fills any missing observations before invoking Cargo.
    observation: dict[str, dict] = {}
    reused_outputs: dict[str, dict] = {}
    binaries = binaries_missing_for_reuse(
        requested_binaries,
        build_env=build_env,
        outputs=outputs,
        variant=variant,
        target_dir=target_dir,
        spec=spec,
        profile=profile,
        reuse_existing=reuse_existing,
        force_rebuild=force_rebuild,
        cargo=cargo,
        release_version=release_version,
        observation=observation,
        reused_outputs=reused_outputs,
    )
    if requested_binaries and not binaries:
        print(
            "package cargo reuse: "
            f"bins={','.join(requested_binaries)} target_dir={target_dir}"
        )

    if binaries:
        # A failed or interrupted rebuild must not leave an older proof reusable.
        previous_stamp = observation.get("stamp") or read_source_build_stamp(target_dir)
        source_build_stamp_path(target_dir).unlink(missing_ok=True)
        if "source" not in observation:
            observation["source"] = source_tree_fingerprint()
        if "recipe" not in observation:
            observation["recipe"] = build_recipe_fingerprint(
                spec=spec,
                profile=profile,
                cargo=cargo,
                release_version=release_version,
                build_env=build_env,
            )
        run_cargo_build(
            cargo,
            spec,
            profile,
            binaries,
            build_env=build_env,
            target_dir=target_dir,
            release_version=release_version,
        )

    validate_source_outputs(outputs)
    if binaries:
        write_source_build_stamp(
            target_dir,
            spec=spec,
            profile=profile,
            variant=variant,
            outputs=outputs,
            proven_binaries=requested_binaries,
            source_before=observation["source"],
            recipe_before=observation["recipe"],
            reused_outputs=reused_outputs,
            previous_stamp=previous_stamp,
            build_env=build_env,
            cargo=cargo,
            release_version=release_version,
        )
    return outputs


def run_cargo_build(
    cargo: str,
    spec: TargetSpec,
    profile: str,
    binaries: list[str],
    *,
    target_dir: Path,
    release_version: str | None = None,
    build_env: dict[str, str] | None = None,
) -> None:
    cargo_env = (
        build_env
        if build_env is not None
        else cargo_build_env(
            spec, profile, target_dir=target_dir, release_version=release_version
        )
    )
    cmd = [
        resolve_command(cargo, env=cargo_env) or cargo,
        "build",
        "--target-dir",
        str(target_dir),
        "--target",
        spec.target,
        "--profile",
        profile,
    ]
    for binary in binaries:
        cmd.extend(["--bin", binary])

    print("+", " ".join(cmd))
    start = time.perf_counter()
    try:
        run_owned(
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
    build_codex_command_runner: bool,
    build_codex_windows_sandbox_setup: bool,
) -> list[str]:
    binaries = []
    if build_entrypoint:
        binaries.append(variant.cargo_bin)
    if build_code_mode_host:
        binaries.append("codex-code-mode-host")
    if build_codex_command_runner:
        binaries.append("codex-command-runner")
    if build_codex_windows_sandbox_setup:
        binaries.append("codex-windows-sandbox-setup")
    return binaries


def validate_prebuilt_executable_targets(
    spec: TargetSpec,
    *,
    entrypoint_bin: Path | None,
    code_mode_host_bin: Path | None,
    codex_command_runner_bin: Path | None,
    codex_windows_sandbox_setup_bin: Path | None,
) -> None:
    expected_machine = {
        "x86_64-pc-windows-msvc": 0x8664,
        "aarch64-pc-windows-msvc": 0xAA64,
    }.get(spec.target)
    if expected_machine is None:
        return
    for role, path in (
        ("entrypoint", entrypoint_bin),
        ("code-mode-host", code_mode_host_bin),
        ("codex-command-runner", codex_command_runner_bin),
        ("codex-windows-sandbox-setup", codex_windows_sandbox_setup_bin),
    ):
        if path is None or not path.is_file():
            continue
        machine = pe_machine(path)
        if machine is None:
            raise RuntimeError(f"Invalid PE executable for prebuilt {role}: {path}")
        if machine != expected_machine:
            raise RuntimeError(
                f"prebuilt {role} target mismatch: {path} has PE machine "
                f"0x{machine:04x}, expected 0x{expected_machine:04x}"
            )


def validate_explicit_output_paths(
    *,
    entrypoint_bin: Path | None,
    code_mode_host_bin: Path | None,
    codex_command_runner_bin: Path | None,
    codex_windows_sandbox_setup_bin: Path | None,
) -> None:
    explicit_paths = [
        ("--entrypoint-bin", "prebuilt entrypoint executable", entrypoint_bin),
        (
            "--code-mode-host-bin",
            "prebuilt code-mode host executable",
            code_mode_host_bin,
        ),
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


def validate_distinct_output_paths(outputs: SourceBuildOutputs) -> None:
    resolved: list[tuple[str, Path]] = []
    for role, path in vars(outputs).items():
        if path is None:
            continue
        canonical = path.resolve()
        for prior_role, prior_path in resolved:
            if canonical == prior_path or (
                canonical.exists()
                and prior_path.exists()
                and canonical.samefile(prior_path)
            ):
                raise RuntimeError(
                    f"{role} and {prior_role} must refer to distinct executables; "
                    f"both resolve to {canonical}"
                )
        resolved.append((role, canonical))


def cargo_profile_output_dir(
    spec: TargetSpec,
    profile: str,
    *,
    target_dir: Path | None = None,
) -> Path:
    target_dir = cargo_target_dir() if target_dir is None else target_dir
    return target_dir / spec.target / cargo_profile_dirname(profile)


def package_target_base(spec: TargetSpec, profile: str) -> Path:
    explicit = os.environ.get(PACKAGE_TARGET_DIR_ENV)
    return (
        resolve_cargo_target_dir(explicit)
        if explicit is not None
        else (
            CODEX_RS_ROOT
            / "target"
            / "package"
            / f"{spec.target}-{cargo_profile_dirname(profile)}"
        )
    )


def cargo_package_target_dir(spec: TargetSpec, profile: str) -> Path:
    base = package_target_base(spec, profile)
    held = (_leased_targets.get() or {}).get((spec.target, profile, str(base.resolve())))
    if held is not None:
        return held
    env = cargo_build_env(spec, profile, target_dir=base)
    identity = effective_tool_contents(spec, env)
    return _target_for_tools(base, identity)


def _target_for_tools(base, identity):
    key = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()[:20]
    return base / f"toolchain-{key}"


def effective_tool_contents(spec, env):
    import tomllib

    build_tools = {}
    linkers = {}
    unavailable = {}
    paths = cargo_config_paths(env)
    for legacy, modern in zip(paths[::2], paths[1::2]):
        # Cargo prefers `config` over `config.toml` in the same directory,
        # and nearer configuration files override ancestors and CARGO_HOME.
        path = legacy if legacy.is_file() else modern
        if not path.is_file():
            continue
        try:
            config = tomllib.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError):
            unavailable[str(path)] = {"status": "unavailable"}
            continue

        def config_command(value, base=path.parent.parent):
            if "/" in value or "\\" in value:
                return str((base / value).resolve())
            return value

        for name in ("rustc", "rustc-wrapper", "rustc-workspace-wrapper"):
            value = config.get("build", {}).get(name)
            if isinstance(value, str):
                build_tools.setdefault(name, config_command(value))
        for target, values in config.get("target", {}).items():
            if isinstance(values, dict) and isinstance(values.get("linker"), str):
                linkers.setdefault(target, config_command(values["linker"]))

    commands = {}
    for name, env_name, default in (
        ("rustc", "RUSTC", "rustc"),
        ("rustc-wrapper", "RUSTC_WRAPPER", ""),
        ("rustc-workspace-wrapper", "RUSTC_WORKSPACE_WRAPPER", ""),
    ):
        value = env.get(
            env_name, env.get(f"CARGO_BUILD_{env_name}", build_tools.get(name, default))
        )
        if value:
            commands[env_name if name != "rustc" else name] = value

    linker_name = cargo_target_linker_env_name(spec.target)
    linker = env.get(linker_name, linkers.get(spec.target))
    if linker is None:
        matches = []
        for target, command in linkers.items():
            if not target.startswith("cfg("):
                continue
            matched = cfg_matches_target(target, spec)
            if matched is None:
                unavailable[target] = {"status": "unavailable"}
            elif matched:
                matches.append(command)
        if len(matches) == 1:
            linker = matches[0]
        elif matches:
            unavailable["linker"] = {"status": "unavailable"}
    if linker:
        commands[linker_name] = linker
    return unavailable | {
        name: executable_content_identity(command, env)
        for name, command in commands.items()
    }


def cfg_matches_target(expression: str, spec: TargetSpec) -> bool | None:
    """Match built-in Windows cfg selectors; unknown predicates cannot prove reuse."""
    facts = {
        "target_arch": "aarch64" if spec.target.startswith("aarch64-") else "x86_64",
        "target_os": "windows",
        "target_family": "windows",
        "target_env": "msvc",
        "target_vendor": "pc",
        "target_pointer_width": "64",
        "target_endian": "little",
    }
    tokens = iter(
        re.findall(r'"(?:[^"\\]|\\.)*"|[A-Za-z_][A-Za-z_0-9]*|\S', expression)
    )
    token = next(tokens, None)

    def parse():
        nonlocal token
        name = token
        token = next(tokens, None)
        if token == "=":
            value = json.loads(next(tokens))
            token = next(tokens, None)
            return facts[name] == value if name in facts else None
        if token != "(":
            return {"windows": True, "unix": False}.get(name)
        token = next(tokens, None)
        values = []
        while token != ")":
            values.append(parse())
            if token != ",":
                break
            token = next(tokens, None)
        if token != ")":
            raise ValueError("invalid cfg selector")
        token = next(tokens, None)
        if name == "all":
            return False if False in values else None if None in values else True
        if name == "any":
            return True if True in values else None if None in values else False
        if name in {"cfg", "not"} and len(values) == 1:
            return values[0] if name == "cfg" or values[0] is None else not values[0]
        return None

    try:
        matched = parse()
        return matched if token is None else None
    except (ValueError, StopIteration, TypeError, RecursionError):
        return None


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
    release_version: str | None = None,
) -> dict[str, str]:
    env = dict(os.environ)
    env.pop("CARGO_TARGET_DIR", None)
    # This is injected into tool processes per chat, not a compiler input.
    # Remove it from execution as well as evidence, leaving arbitrary build
    # script inputs covered by the full environment fingerprint.
    env.pop("CODEX_THREAD_ID", None)
    env.setdefault("RUST_MIN_STACK", DEFAULT_RUST_MIN_STACK)
    if release_version is not None:
        env["CODEX_RELEASE_VERSION"] = release_version
    # Target rustflags are owned by codex-rs/.cargo/config.toml. Do not
    # duplicate that policy here; Cargo applies the checked-in target table.
    if spec.target.endswith("-msvc"):
        linker_env_name = cargo_target_linker_env_name(spec.target)
        if not env.get(linker_env_name):
            lld_link = find_windows_lld_link()
            if lld_link:
                env[linker_env_name] = lld_link
    rustc_wrapper = env.get("RUSTC_WRAPPER")
    if rustc_wrapper is None and shutil.which("sccache"):
        env["RUSTC_WRAPPER"] = "sccache"
        set_sccache_env(env)
    elif rustc_wrapper and is_sccache_wrapper(rustc_wrapper):
        set_sccache_env(env)
    return env


def reject_rustflags_overrides(env: dict[str, str]) -> None:
    # Cargo lets either variable, even when empty, replace every target
    # rustflags table, which would package binaries without the 8 MiB stack and
    # static CRT that codex-rs/.cargo/config.toml supplies. The per-target form
    # joins those tables instead.
    for name in ("RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        if name in env:
            raise RuntimeError(
                f"{name} is set and would replace the target rustflags in "
                "codex-rs/.cargo/config.toml (8 MiB stack, static CRT). Unset it; "
                "add flags with CARGO_TARGET_<TRIPLE>_RUSTFLAGS, which Cargo joins "
                "with them."
            )


def set_sccache_env(env: dict[str, str]) -> None:
    env["SCCACHE_BASEDIRS"] = str(REPO_ROOT.resolve())
    env["SCCACHE_CACHE_SIZE"] = shared_rust_tool_env.sccache_cache_size(env)


def is_sccache_wrapper(value: str) -> bool:
    return shared_rust_tool_env.is_sccache_wrapper(value)


def cargo_target_rustflags_env_name(target: str) -> str:
    return f"CARGO_TARGET_{target.upper().replace('-', '_')}_RUSTFLAGS"


def cargo_target_linker_env_name(target: str) -> str:
    return f"CARGO_TARGET_{target.upper().replace('-', '_')}_LINKER"


def find_windows_lld_link() -> str | None:
    return shared_rust_tool_env.find_windows_lld_link(
        os.environ,
        which=shutil.which,
        default_path=WINDOWS_LLVM_LLD_LINK_DEFAULT,
    )


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
    release_version: str | None = None,
    build_env: dict[str, str] | None = None,
    observation: dict[str, dict] | None = None,
    reused_outputs: dict[str, dict] | None = None,
) -> list[str]:
    if force_rebuild or not reuse_existing:
        return binaries

    stamp = read_source_build_stamp(target_dir, variant=variant)
    if stamp is None:
        return binaries
    if observation is not None:
        observation["stamp"] = stamp

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
    if observation is None:
        observation = {}
    observation["recipe"] = build_recipe_fingerprint(
        spec=spec,
        profile=profile,
        cargo=cargo,
        release_version=release_version,
        build_env=build_env,
    )
    if not source_build_stamp_metadata_matches(
        stamp,
        spec=spec,
        profile=profile,
        variant=variant,
        recipe=observation["recipe"],
    ):
        return binaries
    observation["source"] = source_tree_fingerprint()
    if not source_build_stamp_source_matches(stamp, source=observation["source"]):
        return binaries
    if reused_outputs is not None:
        for binary in binaries:
            if binary not in missing:
                key = source_output_key_for_binary(binary, variant=variant)
                reused_outputs[key] = stamp_outputs[key]
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
    if binary == "codex-command-runner":
        return "codex_command_runner_bin"
    if binary == "codex-windows-sandbox-setup":
        return "codex_windows_sandbox_setup_bin"
    raise RuntimeError(f"unknown source binary output: {binary}")


def validate_source_outputs(outputs: SourceBuildOutputs) -> None:
    for path in [
        outputs.entrypoint_bin,
        outputs.code_mode_host_bin,
        outputs.codex_command_runner_bin,
        outputs.codex_windows_sandbox_setup_bin,
    ]:
        if path is not None and not path.is_file():
            raise RuntimeError(f"cargo build did not produce expected binary: {path}")


def source_build_stamp_path(target_dir: Path) -> Path:
    return target_dir / SOURCE_BUILD_STAMP


def read_source_build_stamp(
    target_dir: Path, *, variant: PackageVariant | None = None
) -> dict | None:
    path = source_build_stamp_path(target_dir)
    if not path.is_file():
        return None
    try:
        stamp = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    if (
        not isinstance(stamp, dict)
        or not isinstance(stamp.get("outputs"), dict)
        or not isinstance(stamp.get("entrypoints", {}), dict)
    ):
        return None
    if variant is not None and "entrypoints" in stamp:
        stamp["outputs"] = {
            **stamp.get("outputs", {}),
            "entrypoint_bin": stamp["entrypoints"].get(variant.name),
        }
    return stamp


def write_source_build_stamp(
    target_dir: Path,
    *,
    spec: TargetSpec,
    profile: str,
    variant: PackageVariant,
    outputs: SourceBuildOutputs,
    cargo: str = "cargo",
    release_version: str | None = None,
    build_env: dict[str, str] | None = None,
    proven_binaries: list[str] | None = None,
    source_before: dict | None = None,
    recipe_before: dict | None = None,
    reused_outputs: dict[str, dict] | None = None,
    previous_stamp: dict | None = None,
) -> None:
    stamp = {
        "target": spec.target,
        "profile": profile,
        "variant": variant.name,
        "build_recipe": build_recipe_fingerprint(
            spec=spec,
            profile=profile,
            cargo=cargo,
            release_version=release_version,
            build_env=build_env,
        ),
        "source": source_tree_fingerprint(),
        "outputs": (
            source_output_fingerprints(outputs)
            if proven_binaries is None
            else {
                source_output_key_for_binary(
                    binary, variant=variant
                ): source_output_fingerprint(
                    expected_output_for_binary(binary, outputs=outputs, variant=variant)
                )
                for binary in proven_binaries
            }
        ),
    }
    path = source_build_stamp_path(target_dir)
    for key, fingerprint in (reused_outputs or {}).items():
        if stamp["outputs"].get(key) != fingerprint:
            path.unlink(missing_ok=True)
            raise RuntimeError(f"package reused output changed during build: {key}")
    if source_before is not None and (
        (source_before.get("status") == "ok" and stamp["source"] != source_before)
        or stamp["build_recipe"] != recipe_before
    ):
        path.unlink(missing_ok=True)
        raise RuntimeError("package build inputs changed during build; retry packaging")
    if source_before is not None and (
        source_before.get("status") != "ok"
        or any(
            value.get("status") == "unavailable"
            for value in (
                stamp["build_recipe"]["cargo"],
                stamp["build_recipe"]["rustc"],
                *stamp["build_recipe"].get("tools", {}).values(),
            )
        )
    ):
        path.unlink(missing_ok=True)
        print(
            "package cargo reuse disabled: build inputs changed or could not be verified"
        )
        return
    entrypoints = {}
    if (
        previous_stamp is not None
        and source_build_stamp_metadata_matches(
            previous_stamp,
            spec=spec,
            profile=profile,
            variant=variant,
            recipe=stamp["build_recipe"],
        )
        and source_build_stamp_source_matches(previous_stamp, source=stamp["source"])
    ):
        for name, fingerprint in previous_stamp.get("entrypoints", {}).items():
            if (
                name == variant.name
                and stamp["outputs"].get("entrypoint_bin") is not None
            ):
                continue
            if (
                isinstance(fingerprint, dict)
                and isinstance(fingerprint.get("path"), str)
                and source_output_matches_fingerprint(
                    Path(fingerprint["path"]), fingerprint
                )
            ):
                entrypoints[name] = fingerprint
        for name, fingerprint in previous_stamp.get("outputs", {}).items():
            if (
                name != "entrypoint_bin"
                and name not in stamp["outputs"]
                and isinstance(fingerprint, dict)
                and isinstance(fingerprint.get("path"), str)
                and source_output_matches_fingerprint(
                    Path(fingerprint["path"]), fingerprint
                )
            ):
                stamp["outputs"][name] = fingerprint
    if stamp["outputs"].get("entrypoint_bin") is not None:
        entrypoints[variant.name] = stamp["outputs"]["entrypoint_bin"]
    stamp["entrypoints"] = entrypoints
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
    release_version: str | None = None,
) -> bool:
    stamp = read_source_build_stamp(target_dir, variant=variant)
    if stamp is None:
        return False

    if not source_build_stamp_metadata_matches(
        stamp,
        spec=spec,
        profile=profile,
        variant=variant,
        cargo=cargo,
        release_version=release_version,
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
    release_version: str | None = None,
    build_env: dict[str, str] | None = None,
    recipe: dict | None = None,
) -> bool:
    if stamp.get("target") != spec.target or stamp.get("profile") != profile:
        return False
    recipe = (
        recipe
        if recipe is not None
        else build_recipe_fingerprint(
            spec=spec,
            profile=profile,
            cargo=cargo,
            release_version=release_version,
            build_env=build_env,
        )
    )
    return (
        all(recipe[tool].get("status") != "unavailable" for tool in ("cargo", "rustc"))
        and all(
            tool.get("status") != "unavailable"
            for tool in recipe.get("tools", {}).values()
        )
        and stamp.get("build_recipe") == recipe
    )


def build_recipe_fingerprint(
    *,
    spec: TargetSpec,
    profile: str,
    cargo: str = "cargo",
    release_version: str | None = None,
    build_env: dict[str, str] | None = None,
) -> dict[str, object]:
    """Capture toolchain and environment inputs that can change Cargo output."""
    base = package_target_base(spec, profile)
    held = (_leased_targets.get() or {}).get((spec.target, profile, str(base.resolve())))
    effective_env = (
        build_env
        if build_env is not None
        else cargo_build_env(
            spec, profile, target_dir=held or base, release_version=release_version
        )
    )
    # Each proof boundary still observes executable contents freshly. Target
    # derivation reuses this observation rather than hashing the tools again.
    tools = effective_tool_contents(spec, effective_env)
    target_dir = held or _target_for_tools(base, tools)
    if build_env is None:
        effective_env = cargo_build_env(
            spec, profile, target_dir=target_dir, release_version=release_version
        )
    rustc = tools.get("rustc", {}).get("path", effective_env.get("RUSTC", "rustc"))
    # Build scripts and env!/option_env! can consume arbitrary variables.
    # Conservatively hash the full supplied environment, including absent vs
    # empty values, rather than treating an allowlist as Cargo freshness proof.
    environment: dict[str, object] = {}
    for name in sorted(effective_env):
        value = effective_env.get(name)
        if value is None:
            continue
        environment[name] = {
            "sha256": hashlib.sha256(value.encode("utf-8")).hexdigest()
        }
    effective_command = [
        cargo,
        "build",
        "--target-dir",
        str(target_dir),
        "--target",
        spec.target,
        "--profile",
        profile,
    ]

    return {
        "schema_version": 7,
        "target": spec.target,
        "profile": profile,
        "cargo": command_identity(cargo, "--version", "--verbose", env=effective_env),
        "rustc": command_identity(rustc, "-Vv", env=effective_env),
        "tools": tools,
        "environment": environment,
        "effective_command_sha256": hashlib.sha256(
            "\0".join(effective_command).encode("utf-8")
        ).hexdigest(),
        "recipe_source": files_fingerprint(
            (
                REPO_ROOT / "scripts" / "codex_package" / "cargo.py",
                REPO_ROOT / "scripts" / "codex_package" / "targets.py",
                REPO_ROOT / "scripts" / "rust_tool_env.py",
                REPO_ROOT / "scripts" / "common-rust-env.ps1",
            )
        ),
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
        "cargo_config": files_fingerprint(cargo_config_paths(effective_env)),
    }


def cargo_config_paths(env: dict[str, str]) -> tuple[Path, ...]:
    # Cargo searches from its invocation directory to the filesystem root,
    # then CARGO_HOME (including the default home when the variable is unset).
    cwd = CODEX_RS_ROOT.resolve()
    home = Path(env.get("CARGO_HOME") or Path.home() / ".cargo")
    if not home.is_absolute():
        home = cwd / home
    directories = [path / ".cargo" for path in (cwd, *cwd.parents)]
    directories.append(home)
    return tuple(
        directory / name
        for directory in directories
        for name in ("config", "config.toml")
    )


def resolve_command(command: str, *, env: dict[str, str] | None = None) -> str | None:
    """Use the build environment for both discovery and execution on Windows."""
    effective_env = os.environ if env is None else env
    cwd = CODEX_RS_ROOT.resolve()
    command_path = Path(command)
    if command_path.is_absolute() or "/" in command or "\\" in command:
        executable = str((cwd / command_path).resolve())
    else:
        search_directories = [
            (cwd / entry).resolve()
            for entry in effective_env.get("PATH", os.defpath).split(os.pathsep)
        ]
        if os.name == "nt":
            search_directories.insert(0, cwd)
        # Absolute candidates prevent shutil.which on Windows from inserting
        # the parent process's working directory ahead of the supplied PATH.
        executable = next(
            (
                found
                for directory in search_directories
                if (found := shutil.which(str(directory / command)))
            ),
            None,
        )
    return executable


def command_identity(
    command: str, *args: str, env: dict[str, str] | None = None
) -> dict[str, object]:
    executable = resolve_command(command, env=env)
    if executable is None:
        return {"path": command, "status": "unavailable", "error": "tool not found"}
    try:
        completed = subprocess.run(
            [executable, *args],
            check=True,
            cwd=CODEX_RS_ROOT,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            timeout=DISCOVERY_TIMEOUT_SECONDS,
        )
    except (OSError, subprocess.SubprocessError) as error:
        return {"path": executable, "status": "unavailable", "error": str(error)}
    return {
        "path": str(Path(executable).resolve()),
        "version": completed.stdout.strip(),
        **executable_content_identity(
            executable, env or dict(os.environ), resolved=True
        ),
    }


def executable_content_identity(command, env, *, resolved=False, _seen=frozenset()):
    executable = command if resolved else resolve_command(command, env=env)
    if executable is None or not Path(executable).is_file():
        return {"status": "unavailable", "path": command}
    path = Path(executable)
    canonical = path.resolve()
    if canonical in _seen or len(_seen) >= 16:
        return {"status": "unavailable", "path": str(canonical)}
    try:
        with path.open("rb") as handle:
            content = hashlib.file_digest(handle, "sha256").hexdigest()
        identity = {"path": str(canonical), "sha256": content}
        shim = path.with_suffix(".shim")
        if path.suffix.lower() == ".exe" and shim.is_file():
            dispatch = shim.read_bytes()
            identity["shim_sha256"] = hashlib.sha256(dispatch).hexdigest()
            fields = dict(
                re.findall(
                    r'^\s*(\w+)\s*=\s*"(.*)"\s*$',
                    dispatch.decode("utf-8-sig"),
                    re.MULTILINE,
                )
            )
            target = Path(fields.get("path", ""))
            if not target.is_absolute():
                identity["status"] = "unavailable"
                return identity
            target_identity = executable_content_identity(
                str(target), env, resolved=True, _seen=_seen | {canonical}
            )
            identity["shim_target"] = target_identity
            # Argument-bearing launchers can load arbitrary scripts/configs.
            # Their dispatch changes partition Cargo outputs, but opaque inputs
            # must not authorize the fingerprint-only skip path.
            if fields.get("args") or target_identity.get("status") == "unavailable":
                identity["status"] = "unavailable"
        return identity
    except (OSError, UnicodeError):
        return {"status": "unavailable", "path": str(canonical)}


def files_fingerprint(paths: tuple[Path, ...]) -> dict[str, object]:
    return {
        (
            str(path.relative_to(REPO_ROOT))
            if path.is_relative_to(REPO_ROOT)
            else str(path)
        ): source_output_fingerprint(path)
        for path in paths
        if path.is_file()
    }


def source_build_stamp_source_matches(
    stamp: dict, *, source: dict | None = None
) -> bool:
    source = source if source is not None else source_tree_fingerprint()
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
                with file_path.open("rb") as source:
                    for chunk in iter(lambda: source.read(1024 * 1024), b""):
                        untracked_contents.update(chunk)
            except OSError:
                return {"status": "unavailable", "reason": "unreadable-source"}
            untracked_contents.update(b"\0")
    except (OSError, subprocess.SubprocessError):
        return {"status": "unavailable", "reason": "git-unavailable"}

    return {
        "status": "ok",
        "git_head": head,
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
    try:
        stdout, _ = process.communicate(timeout=DISCOVERY_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired:
        process.kill()
        process.communicate(timeout=DISCOVERY_TIMEOUT_SECONDS)
        raise
    if process.returncode != 0:
        raise subprocess.CalledProcessError(process.returncode, [git, *args])
    return stdout


def source_output_fingerprints(outputs: SourceBuildOutputs) -> dict[str, dict | None]:
    return {
        "entrypoint_bin": source_output_fingerprint(outputs.entrypoint_bin),
        "code_mode_host_bin": source_output_fingerprint(outputs.code_mode_host_bin),
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
            "codex_command_runner_bin": outputs.codex_command_runner_bin,
            "codex_windows_sandbox_setup_bin": outputs.codex_windows_sandbox_setup_bin,
        }.items()
    )


def source_output_matches_fingerprint(path: Path | None, fingerprint: object) -> bool:
    if path is None:
        return fingerprint is None
    if not isinstance(fingerprint, dict) or not path.is_file():
        return False
    stat = path.stat()
    if any(
        fingerprint.get(key) != value
        for key, value in {
            "path": str(path),
            "size": stat.st_size,
        }.items()
    ):
        return False
    return fingerprint == source_output_fingerprint(path)
