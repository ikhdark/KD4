#!/usr/bin/env python3
"""Stage and optionally package the @openai/codex npm module."""

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
CODEX_CLI_ROOT = SCRIPT_DIR.parent
REPO_ROOT = CODEX_CLI_ROOT.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from scripts.codex_package.targets import NPM_TARGETS  # noqa: E402

RESPONSES_API_PROXY_NPM_ROOT = REPO_ROOT / "codex-rs" / "responses-api-proxy" / "npm"
CODEX_SDK_ROOT = REPO_ROOT / "sdk" / "typescript"
CODEX_NPM_NAME = "@openai/codex"
CODEX_PACKAGE_COMPONENT = "codex-package"

# Derived adapter for the staging API. Canonical target metadata lives in
# scripts/codex_package/targets.py.
CODEX_PLATFORM_PACKAGES: dict[str, dict[str, str]] = {
    target.package: {
        "npm_name": target.npm_name,
        "npm_tag": target.npm_tag,
        "target_triple": target_triple,
        "os": target.node_platform,
        "cpu": target.node_arch,
        "platform_key": target.node_platform_key,
        "executable_name": target.executable_name,
    }
    for target_triple, target in NPM_TARGETS.items()
}

PACKAGE_EXPANSIONS: dict[str, list[str]] = {
    "codex": ["codex", *CODEX_PLATFORM_PACKAGES],
}

PACKAGE_NATIVE_COMPONENTS: dict[str, list[str]] = {
    "codex": [],
    **{
        package_name: [CODEX_PACKAGE_COMPONENT]
        for package_name in CODEX_PLATFORM_PACKAGES
    },
    "codex-responses-api-proxy": ["codex-responses-api-proxy"],
    "codex-sdk": [],
}

PACKAGE_TARGET_FILTERS: dict[str, str] = {
    package_name: package_config["target_triple"]
    for package_name, package_config in CODEX_PLATFORM_PACKAGES.items()
}

PACKAGE_CHOICES = tuple(PACKAGE_NATIVE_COMPONENTS)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build or stage the Codex CLI npm package."
    )
    parser.add_argument(
        "--package",
        choices=PACKAGE_CHOICES,
        default="codex",
        help="Which npm package to stage (default: codex).",
    )
    parser.add_argument(
        "--version",
        help="Version number to write to package.json inside the staged package.",
    )
    parser.add_argument(
        "--release-version",
        help=("Version to stage for npm release."),
    )
    parser.add_argument(
        "--staging-dir",
        type=Path,
        help=(
            "Directory to stage the package contents. Defaults to a new temporary directory "
            "if omitted. The directory must be empty when provided."
        ),
    )
    parser.add_argument(
        "--tmp",
        dest="staging_dir",
        type=Path,
        help=argparse.SUPPRESS,
    )
    parser.add_argument(
        "--pack-output",
        type=Path,
        help="Path where the generated npm tarball should be written.",
    )
    parser.add_argument(
        "--vendor-src",
        type=Path,
        help="Directory containing pre-installed native binaries to bundle (vendor root).",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()

    package = args.package
    version = args.version
    release_version = args.release_version
    if release_version:
        if version and version != release_version:
            raise RuntimeError(
                "--version and --release-version must match when both are provided."
            )
        version = release_version

    if not version:
        raise RuntimeError("Must specify --version or --release-version.")

    staging_dir, created_temp = prepare_staging_dir(args.staging_dir)

    try:
        stage_sources(staging_dir, version, package)

        vendor_src = args.vendor_src.resolve() if args.vendor_src else None
        native_components = PACKAGE_NATIVE_COMPONENTS.get(package, [])
        target_filter = PACKAGE_TARGET_FILTERS.get(package)

        if native_components:
            if vendor_src is None:
                components_str = ", ".join(native_components)
                raise RuntimeError(
                    "Native components "
                    f"({components_str}) required for package '{package}'. Provide --vendor-src "
                    "pointing to a directory containing pre-installed binaries."
                )

            copy_native_binaries(
                vendor_src,
                staging_dir,
                native_components,
                target_filter={target_filter} if target_filter else None,
                expected_version=version,
            )

        if release_version:
            staging_dir_str = str(staging_dir)
            if package == "codex":
                print(
                    f"Staged version {version} for release in {staging_dir_str}\n\n"
                    "Verify the CLI:\n"
                    f"    node {staging_dir_str}/bin/codex.js --version\n"
                    f"    node {staging_dir_str}/bin/codex.js --help\n\n"
                )
            elif package == "codex-responses-api-proxy":
                print(
                    f"Staged version {version} for release in {staging_dir_str}\n\n"
                    "Verify the responses API proxy:\n"
                    f"    node {staging_dir_str}/bin/codex-responses-api-proxy.js --help\n\n"
                )
            elif package in CODEX_PLATFORM_PACKAGES:
                print(
                    f"Staged version {version} for release in {staging_dir_str}\n\n"
                    "Verify native payload contents:\n"
                    f"    ls {staging_dir_str}/vendor\n\n"
                )
            else:
                print(
                    f"Staged version {version} for release in {staging_dir_str}\n\n"
                    "Verify the SDK contents:\n"
                    f"    ls {staging_dir_str}/dist\n"
                    "    node -e \"import('./dist/index.js').then(() => console.log('ok'))\"\n\n"
                )
        else:
            print(f"Staged package in {staging_dir}")

        if args.pack_output is not None:
            output_path = run_npm_pack(staging_dir, args.pack_output)
            print(f"npm pack output written to {output_path}")
    finally:
        if created_temp:
            # Preserve the staging directory for further inspection.
            pass

    return 0


def prepare_staging_dir(staging_dir: Path | None) -> tuple[Path, bool]:
    if staging_dir is not None:
        staging_dir = staging_dir.resolve()
        staging_dir.mkdir(parents=True, exist_ok=True)
        if any(staging_dir.iterdir()):
            raise RuntimeError(f"Staging directory {staging_dir} is not empty.")
        return staging_dir, False

    temp_dir = Path(tempfile.mkdtemp(prefix="codex-npm-stage-"))
    return temp_dir, True


def stage_sources(staging_dir: Path, version: str, package: str) -> None:
    package_json: dict
    package_json_path: Path | None = None

    if package == "codex":
        bin_dir = staging_dir / "bin"
        bin_dir.mkdir(parents=True, exist_ok=True)
        shutil.copy2(CODEX_CLI_ROOT / "bin" / "codex.js", bin_dir / "codex.js")

        readme_src = CODEX_CLI_ROOT / "README.npm.md"
        if readme_src.exists():
            shutil.copy2(readme_src, staging_dir / "README.md")

        package_json = build_codex_package_json(version)
    elif package in CODEX_PLATFORM_PACKAGES:
        platform_package = CODEX_PLATFORM_PACKAGES[package]
        platform_npm_tag = platform_package["npm_tag"]
        platform_version = compute_platform_package_version(version, platform_npm_tag)

        readme_src = CODEX_CLI_ROOT / "README.npm.md"
        if readme_src.exists():
            shutil.copy2(readme_src, staging_dir / "README.md")

        package_json = build_platform_package_json(platform_version, platform_package)
    elif package == "codex-responses-api-proxy":
        bin_dir = staging_dir / "bin"
        bin_dir.mkdir(parents=True, exist_ok=True)
        launcher_src = (
            RESPONSES_API_PROXY_NPM_ROOT / "bin" / "codex-responses-api-proxy.js"
        )
        shutil.copy2(launcher_src, bin_dir / "codex-responses-api-proxy.js")

        readme_src = RESPONSES_API_PROXY_NPM_ROOT / "README.md"
        if readme_src.exists():
            shutil.copy2(readme_src, staging_dir / "README.md")

        package_json_path = RESPONSES_API_PROXY_NPM_ROOT / "package.json"
    elif package == "codex-sdk":
        package_json_path = CODEX_SDK_ROOT / "package.json"
        stage_codex_sdk_sources(staging_dir)
    else:
        raise RuntimeError(f"Unknown package '{package}'.")

    if package_json_path is not None:
        with open(package_json_path, "r", encoding="utf-8") as fh:
            package_json = json.load(fh)
        package_json["version"] = version
        package_json["os"] = ["win32"]

    if package == "codex-sdk":
        scripts = package_json.get("scripts")
        if isinstance(scripts, dict):
            scripts.pop("prepare", None)

        dependencies = package_json.get("dependencies")
        if not isinstance(dependencies, dict):
            dependencies = {}
        dependencies[CODEX_NPM_NAME] = version
        package_json["dependencies"] = dependencies

    for notice_name in ("LICENSE", "NOTICE"):
        shutil.copy2(REPO_ROOT / notice_name, staging_dir / notice_name)
    files = package_json.get("files")
    if isinstance(files, list):
        for notice_name in ("LICENSE", "NOTICE"):
            if notice_name not in files:
                files.append(notice_name)

    with open(staging_dir / "package.json", "w", encoding="utf-8") as out:
        json.dump(package_json, out, indent=2)
        out.write("\n")


def compute_platform_package_version(version: str, platform_tag: str) -> str:
    # npm forbids republishing the same package name/version, so each
    # platform-specific tarball needs a unique version string.
    return f"{version}-{platform_tag}"


def build_codex_package_json(version: str) -> dict:
    with open(CODEX_CLI_ROOT / "package.json", "r", encoding="utf-8") as fh:
        package_json = json.load(fh)
    package_json.pop("private", None)
    package_json["version"] = version
    package_json["files"] = ["bin/codex.js", "LICENSE", "NOTICE"]
    package_json["os"] = ["win32"]
    package_json["codexNativeTargets"] = {
        platform_package["platform_key"]: {
            "targetTriple": platform_package["target_triple"],
            "package": platform_package["npm_name"],
            "binary": platform_package["executable_name"],
        }
        for platform_package in CODEX_PLATFORM_PACKAGES.values()
    }
    package_json["optionalDependencies"] = {
        CODEX_PLATFORM_PACKAGES[platform_package]["npm_name"]: (
            f"npm:{CODEX_NPM_NAME}@"
            f"{compute_platform_package_version(version, CODEX_PLATFORM_PACKAGES[platform_package]['npm_tag'])}"
        )
        for platform_package in PACKAGE_EXPANSIONS["codex"]
        if platform_package != "codex"
    }
    return package_json


def build_platform_package_json(version: str, platform_package: dict[str, str]) -> dict:
    with open(CODEX_CLI_ROOT / "package.json", "r", encoding="utf-8") as fh:
        codex_package_json = json.load(fh)

    package_json = {
        "name": CODEX_NPM_NAME,
        "version": version,
        "license": codex_package_json.get("license", "Apache-2.0"),
        "os": [platform_package["os"]],
        "cpu": [platform_package["cpu"]],
        "files": ["vendor", "LICENSE", "NOTICE"],
        "repository": codex_package_json.get("repository"),
    }
    engines = codex_package_json.get("engines")
    if isinstance(engines, dict):
        package_json["engines"] = engines
    return package_json


def run_command(cmd: list[str], cwd: Path | None = None) -> None:
    print("+", " ".join(cmd), flush=True)
    subprocess.run(cmd, cwd=cwd, check=True)


def stage_codex_sdk_sources(staging_dir: Path) -> None:
    package_root = CODEX_SDK_ROOT

    run_command(["pnpm", "install", "--frozen-lockfile"], cwd=package_root)
    run_command(["pnpm", "run", "build"], cwd=package_root)

    dist_src = package_root / "dist"
    if not dist_src.exists():
        raise RuntimeError("codex-sdk build did not produce a dist directory.")

    shutil.copytree(dist_src, staging_dir / "dist")

    readme_src = package_root / "README.md"
    if readme_src.exists():
        shutil.copy2(readme_src, staging_dir / "README.md")

    license_src = REPO_ROOT / "LICENSE"
    if license_src.exists():
        shutil.copy2(license_src, staging_dir / "LICENSE")


def copy_native_binaries(
    vendor_src: Path,
    staging_dir: Path,
    components: list[str],
    target_filter: set[str] | None = None,
    expected_version: str | None = None,
) -> None:
    vendor_src = vendor_src.resolve()
    if not vendor_src.exists():
        raise RuntimeError(f"Vendor source directory not found: {vendor_src}")

    components_set = set(components)
    if not components_set:
        return

    vendor_dest = staging_dir / "vendor"
    if vendor_dest.exists():
        shutil.rmtree(vendor_dest)
    vendor_dest.mkdir(parents=True, exist_ok=True)

    copied_targets: set[str] = set()

    for target_dir in vendor_src.iterdir():
        if not target_dir.is_dir():
            continue

        if "windows" not in target_dir.name:
            raise RuntimeError(
                f"Unsupported non-Windows native target in vendor source: {target_dir.name}"
            )

        if target_filter is not None and target_dir.name not in target_filter:
            continue

        copied_targets.add(target_dir.name)

        dest_target_dir = vendor_dest / target_dir.name

        if CODEX_PACKAGE_COMPONENT in components_set:
            codex_path = target_dir / "bin" / "codex.exe"
            if not codex_path.is_file():
                raise RuntimeError(
                    f"Missing Codex executable for {target_dir.name}: {codex_path}"
                )
            metadata_path = target_dir / "codex-package.json"
            if not metadata_path.is_file():
                raise RuntimeError(f"Missing canonical package metadata: {metadata_path}")
            metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
            if metadata.get("target") != target_dir.name:
                raise RuntimeError(
                    f"Canonical package target mismatch for {target_dir.name}"
                )
            if expected_version is not None and metadata.get("version") != expected_version:
                raise RuntimeError(
                    f"Canonical package version mismatch for {target_dir.name}: "
                    f"expected {expected_version}, got {metadata.get('version')!r}"
                )
            inventory = metadata.get("files")
            if not isinstance(inventory, list) or not inventory:
                raise RuntimeError(f"Canonical package has no file inventory: {metadata_path}")
            declared = {"codex-package.json"}
            for item in inventory:
                if not isinstance(item, dict) or not isinstance(item.get("path"), str):
                    raise RuntimeError(f"Invalid canonical package inventory: {metadata_path}")
                relative = item["path"]
                relative_path = Path(relative)
                if relative_path.is_absolute() or ".." in relative_path.parts:
                    raise RuntimeError(
                        f"Unsafe canonical package inventory path: {relative!r}"
                    )
                source = target_dir / relative_path
                if not source.is_file():
                    raise RuntimeError(f"Missing canonical package file: {source}")
                actual_size = source.stat().st_size
                actual_digest = hashlib.sha256(source.read_bytes()).hexdigest()
                if item.get("size") != actual_size or item.get("sha256") != actual_digest:
                    raise RuntimeError(
                        f"Canonical package digest mismatch: {target_dir.name}/{relative}"
                    )
                declared.add(relative)
            expected_bundle_id = hashlib.sha256(
                json.dumps(
                    inventory, sort_keys=True, separators=(",", ":")
                ).encode("utf-8")
            ).hexdigest()
            if metadata.get("bundleId") != expected_bundle_id:
                raise RuntimeError(
                    f"Canonical package bundle identity mismatch: {metadata_path}"
                )
            actual = {
                path.relative_to(target_dir).as_posix()
                for path in target_dir.rglob("*")
                if path.is_file()
            }
            if actual != declared:
                raise RuntimeError(
                    f"Canonical package inventory mismatch for {target_dir.name}: "
                    f"unexpected={sorted(actual - declared)}, missing={sorted(declared - actual)}"
                )
            for relative in sorted(declared):
                source = target_dir / relative
                destination = dest_target_dir / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, destination)

            host_target = {
                "AMD64": "x86_64-pc-windows-msvc",
                "X86_64": "x86_64-pc-windows-msvc",
                "ARM64": "aarch64-pc-windows-msvc",
                "AARCH64": "aarch64-pc-windows-msvc",
            }.get(platform.machine().upper())
            if expected_version is not None and os.name == "nt" and host_target == target_dir.name:
                reported = subprocess.check_output(
                    [str(codex_path), "--version"], text=True
                ).strip().split()[-1]
                if reported != expected_version:
                    raise RuntimeError(
                        f"Native Codex version mismatch for {target_dir.name}: "
                        f"expected {expected_version}, got {reported}"
                    )
        else:
            dest_target_dir.mkdir(parents=True, exist_ok=True)

        for component in sorted(components_set - {CODEX_PACKAGE_COMPONENT}):
            src_component_dir = target_dir / component
            if not src_component_dir.exists():
                raise RuntimeError(
                    f"Missing native component '{component}' in vendor source: {src_component_dir}"
                )

            dest_component_dir = dest_target_dir / component
            if dest_component_dir.exists():
                shutil.rmtree(dest_component_dir)
            shutil.copytree(src_component_dir, dest_component_dir)

    if target_filter is not None:
        missing_targets = sorted(target_filter - copied_targets)
        if missing_targets:
            missing_list = ", ".join(missing_targets)
            raise RuntimeError(
                f"Missing target directories in vendor source: {missing_list}"
            )


def run_npm_pack(staging_dir: Path, output_path: Path) -> Path:
    output_path = output_path.resolve()
    output_path.parent.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="codex-npm-pack-") as pack_dir_str:
        pack_dir = Path(pack_dir_str)
        npm_cache_dir = pack_dir / "npm-cache"
        npm_logs_dir = pack_dir / "npm-logs"
        npm_cache_dir.mkdir()
        npm_logs_dir.mkdir()
        env = os.environ.copy()
        env["NPM_CONFIG_CACHE"] = str(npm_cache_dir)
        env["NPM_CONFIG_LOGS_DIR"] = str(npm_logs_dir)
        stdout = subprocess.check_output(
            ["npm", "pack", "--json", "--pack-destination", str(pack_dir)],
            cwd=staging_dir,
            env=env,
            text=True,
        )
        try:
            pack_output = json.loads(stdout)
        except json.JSONDecodeError as exc:
            raise RuntimeError("Failed to parse npm pack output.") from exc

        if not pack_output:
            raise RuntimeError("npm pack did not produce an output tarball.")

        tarball_name = pack_output[0].get("filename") or pack_output[0].get("name")
        if not tarball_name:
            raise RuntimeError("Unable to determine npm pack output filename.")

        tarball_path = pack_dir / tarball_name
        if not tarball_path.exists():
            raise RuntimeError(f"Expected npm pack output not found: {tarball_path}")

        shutil.move(str(tarball_path), output_path)

    smoke_test_npm_tarball(output_path)
    return output_path


def smoke_test_npm_tarball(tarball_path: Path) -> None:
    with tempfile.TemporaryDirectory(prefix="codex-npm-smoke-") as smoke_dir_str:
        smoke_dir = Path(smoke_dir_str)
        subprocess.run(
            [
                "npm",
                "install",
                "--force",
                "--ignore-scripts",
                "--omit=optional",
                "--no-package-lock",
                str(tarball_path),
            ],
            cwd=smoke_dir,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        installed = smoke_dir / "node_modules" / "@openai" / "codex"
        package_json = installed / "package.json"
        if not package_json.is_file():
            raise RuntimeError("npm smoke install did not produce @openai/codex/package.json")
        launcher = installed / "bin" / "codex.js"
        if launcher.is_file():
            subprocess.run(["node", "--check", str(launcher)], check=True)


if __name__ == "__main__":
    import sys

    sys.exit(main())
