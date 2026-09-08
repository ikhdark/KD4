import ast
import importlib.util
import io
import os
import sys
import tarfile
import urllib.error
from pathlib import Path

import pytest
import tomllib
from pydantic import ValidationError

ROOT = Path(__file__).resolve().parents[1]


def _load_update_script_module():
    """Load the maintenance script as a module so tests exercise real helpers."""
    script_path = ROOT / "scripts" / "update_sdk_artifacts.py"
    spec = importlib.util.spec_from_file_location("update_sdk_artifacts", script_path)
    if spec is None or spec.loader is None:
        raise AssertionError(f"Failed to load script module: {script_path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _load_runtime_setup_module():
    """Load runtime setup without importing the SDK package under test."""
    runtime_setup_path = ROOT / "_runtime_setup.py"
    spec = importlib.util.spec_from_file_location("_runtime_setup", runtime_setup_path)
    if spec is None or spec.loader is None:
        raise AssertionError(f"Failed to load runtime setup module: {runtime_setup_path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def _write_fake_codex_package(package_dir: Path, script) -> Path:
    (package_dir / "bin").mkdir(parents=True)
    (package_dir / "codex-resources").mkdir()
    (package_dir / "codex-path").mkdir()
    (package_dir / "codex-package.json").write_text('{"variant":"codex"}\n')
    (package_dir / "bin" / script.runtime_binary_name()).write_text("fake codex\n")
    (package_dir / "bin" / script.runtime_code_mode_host_name()).write_text("fake code mode host\n")
    (package_dir / "codex-path" / "rg").write_text("fake rg\n")
    return package_dir


def _write_fake_codex_package_archive(tmp_path: Path, script) -> Path:
    package_dir = _write_fake_codex_package(tmp_path / "codex-package", script)
    archive_path = tmp_path / "codex-package.tar.gz"
    _write_package_archive(package_dir, archive_path)
    return archive_path


def _write_package_archive(package_dir: Path, archive_path: Path) -> None:
    with tarfile.open(archive_path, "w:gz") as archive:
        for path in package_dir.rglob("*"):
            archive.add(path, arcname=path.relative_to(package_dir))


def test_generation_has_single_maintenance_entrypoint_script() -> None:
    """Keep artifact workflows routed through one script instead of side entrypoints."""
    scripts = sorted(p.name for p in (ROOT / "scripts").glob("*.py"))
    assert scripts == ["update_sdk_artifacts.py"]


def test_generated_chatgpt_account_email_is_required_nullable() -> None:
    from openai_codex.generated.v2_all import ChatgptAccount

    account = ChatgptAccount.model_validate({"email": None, "planType": "pro", "type": "chatgpt"})
    assert account.email is None
    assert ChatgptAccount.model_fields["email"].is_required()

    with pytest.raises(ValidationError):
        ChatgptAccount.model_validate({"planType": "pro", "type": "chatgpt"})


def test_runtime_package_template_has_no_checked_in_binaries() -> None:
    runtime_root = ROOT.parent / "python-runtime" / "src" / "codex_cli_bin"
    assert sorted(
        path.name
        for path in runtime_root.rglob("*")
        if path.is_file() and "__pycache__" not in path.parts
    ) == ["__init__.py"]


def test_examples_readme_points_to_runtime_version_source_of_truth() -> None:
    """Document that examples should point at the dependency pin, not release lore."""
    readme = (ROOT / "examples" / "README.md").read_text()
    assert "The pinned runtime version comes from the SDK package dependency." in readme


def test_runtime_distribution_name_is_consistent() -> None:
    script = _load_update_script_module()
    runtime_setup = _load_runtime_setup_module()
    from openai_codex import _version, client as client_module

    assert script.SDK_DISTRIBUTION_NAME == "openai-codex"
    assert runtime_setup.SDK_PACKAGE_NAME == "openai-codex"
    assert _version.DISTRIBUTION_NAME == "openai-codex"
    assert script.RUNTIME_DISTRIBUTION_NAME == "openai-codex-cli-bin"
    assert runtime_setup.PACKAGE_NAME == "openai-codex-cli-bin"
    assert client_module.RUNTIME_PKG_NAME == "openai-codex-cli-bin"
    assert (
        "importlib.metadata.version('codex-cli-bin')"
        not in (ROOT / "_runtime_setup.py").read_text()
    )


def test_source_sdk_template_pins_published_runtime() -> None:
    """The source template should carry a development version and reviewed runtime pin."""
    script = _load_update_script_module()
    pyproject = tomllib.loads((ROOT / "pyproject.toml").read_text())

    assert {
        "sdk_template_version": pyproject["project"]["version"],
        "runtime_pin": script.pinned_runtime_version(),
        "dependencies": pyproject["project"]["dependencies"],
    } == {
        "sdk_template_version": "0.0.0-dev",
        "runtime_pin": "0.149.0",
        "dependencies": [
            "pydantic>=2.12",
            "openai-codex-cli-bin==0.149.0",
        ],
    }


def test_source_sdk_package_declares_beta_documentation() -> None:
    """Public package metadata should link beta docs."""
    pyproject = tomllib.loads((ROOT / "pyproject.toml").read_text())
    readme = (ROOT / "README.md").read_text()

    assert {
        "description": pyproject["project"]["description"],
        "is_beta": "Development Status :: 4 - Beta" in pyproject["project"]["classifiers"],
        "license": pyproject["project"]["license"],
        "documentation": pyproject["project"]["urls"]["Documentation"],
        "readme_is_beta": "# OpenAI Codex Python SDK (Beta)" in readme,
        "local_license_file": (ROOT / "LICENSE").exists(),
    } == {
        "description": "Python SDK for Codex",
        "is_beta": True,
        "license": "Apache-2.0",
        "documentation": "https://github.com/openai/codex/tree/main/sdk/python/docs",
        "readme_is_beta": True,
        "local_license_file": False,
    }


def test_release_metadata_retries_without_invalid_auth(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    runtime_setup = _load_runtime_setup_module()
    authorizations: list[str | None] = []

    def fake_urlopen(request):
        authorization = request.headers.get("Authorization")
        authorizations.append(authorization)
        if authorization is not None:
            raise urllib.error.HTTPError(
                request.full_url,
                401,
                "Unauthorized",
                hdrs=None,
                fp=None,
            )
        return io.StringIO('{"assets": []}')

    monkeypatch.setenv("GH_TOKEN", "invalid-token")
    monkeypatch.setattr(runtime_setup.urllib.request, "urlopen", fake_urlopen)

    assert runtime_setup._release_metadata("1.2.3") == {"assets": []}
    assert authorizations == ["Bearer invalid-token", None]


def test_runtime_setup_reads_independent_runtime_pin_and_release_tags() -> None:
    """Runtime package pins remain independent of the SDK beta version."""
    runtime_setup = _load_runtime_setup_module()
    pyproject = tomllib.loads((ROOT / "pyproject.toml").read_text())

    assert {
        "package_name": runtime_setup.PACKAGE_NAME,
        "release_repository": runtime_setup.REPO_SLUG,
        "sdk_template_version": pyproject["project"]["version"],
        "runtime_pin": runtime_setup.pinned_runtime_version(),
        "normalized_release_version": runtime_setup._normalized_package_version(
            "rust-v0.116.0-alpha.1"
        ),
        "release_tag": runtime_setup._release_tag("0.116.0a1"),
    } == {
        "package_name": "openai-codex-cli-bin",
        "release_repository": "ikhdark/KD4",
        "sdk_template_version": "0.0.0-dev",
        "runtime_pin": "0.149.0",
        "normalized_release_version": "0.116.0a1",
        "release_tag": "rust-v0.116.0-alpha.1",
    }


@pytest.mark.parametrize(
    ("system", "machine", "asset_name"),
    [
        ("Windows", "arm64", "codex-package-aarch64-pc-windows-msvc.tar.gz"),
        ("Windows", "AMD64", "codex-package-x86_64-pc-windows-msvc.tar.gz"),
    ],
)
def test_runtime_setup_downloads_codex_package_archives(
    monkeypatch: pytest.MonkeyPatch,
    system: str,
    machine: str,
    asset_name: str,
) -> None:
    runtime_setup = _load_runtime_setup_module()
    monkeypatch.setattr(runtime_setup.platform, "system", lambda: system)
    monkeypatch.setattr(runtime_setup.platform, "machine", lambda: machine)

    assert runtime_setup.platform_asset_name() == asset_name


@pytest.mark.parametrize("system", ["Darwin", "Linux"])
def test_runtime_setup_rejects_non_windows_hosts(
    monkeypatch: pytest.MonkeyPatch,
    system: str,
) -> None:
    runtime_setup = _load_runtime_setup_module()
    monkeypatch.setattr(runtime_setup.platform, "system", lambda: system)
    monkeypatch.setattr(runtime_setup.platform, "machine", lambda: "x86_64")

    with pytest.raises(runtime_setup.RuntimeSetupError, match="Windows is required"):
        runtime_setup.platform_asset_name()


@pytest.mark.parametrize(
    ("machine", "platform_tag"),
    [("AMD64", "win_amd64"), ("arm64", "win_arm64")],
)
def test_runtime_setup_selects_windows_wheel_tag(
    monkeypatch: pytest.MonkeyPatch,
    machine: str,
    platform_tag: str,
) -> None:
    runtime_setup = _load_runtime_setup_module()
    monkeypatch.setattr(runtime_setup.platform, "system", lambda: "Windows")
    monkeypatch.setattr(runtime_setup.platform, "machine", lambda: machine)

    assert runtime_setup.platform_wheel_tag() == platform_tag


def test_runtime_package_is_wheel_only_and_builds_platform_specific_wheels() -> None:
    pyproject = tomllib.loads((ROOT.parent / "python-runtime" / "pyproject.toml").read_text())
    hook_source = (ROOT.parent / "python-runtime" / "hatch_build.py").read_text()
    hook_tree = ast.parse(hook_source)
    initialize_fn = next(
        node
        for node in ast.walk(hook_tree)
        if isinstance(node, ast.FunctionDef) and node.name == "initialize"
    )

    sdist_guard = next(
        (
            node
            for node in initialize_fn.body
            if isinstance(node, ast.If)
            and isinstance(node.test, ast.Compare)
            and isinstance(node.test.left, ast.Attribute)
            and isinstance(node.test.left.value, ast.Name)
            and node.test.left.value.id == "self"
            and node.test.left.attr == "target_name"
            and len(node.test.ops) == 1
            and isinstance(node.test.ops[0], ast.Eq)
            and len(node.test.comparators) == 1
            and isinstance(node.test.comparators[0], ast.Constant)
            and node.test.comparators[0].value == "sdist"
        ),
        None,
    )
    build_data_assignments = {}
    for node in initialize_fn.body:
        if (
            not isinstance(node, ast.Assign)
            or len(node.targets) != 1
            or not isinstance(node.targets[0], ast.Subscript)
            or not isinstance(node.targets[0].value, ast.Name)
            or node.targets[0].value.id != "build_data"
            or not isinstance(node.targets[0].slice, ast.Constant)
            or not isinstance(node.targets[0].slice.value, str)
        ):
            continue
        if isinstance(node.value, ast.Constant):
            build_data_assignments[node.targets[0].slice.value] = node.value.value
        elif isinstance(node.value, ast.JoinedStr):
            build_data_assignments[node.targets[0].slice.value] = "joined-string"

    assert pyproject["project"]["name"] == "openai-codex-cli-bin"
    assert pyproject["tool"]["hatch"]["build"]["targets"]["wheel"] == {
        "packages": ["src/codex_cli_bin"],
        "include": [
            "src/codex_cli_bin/codex-package.json",
            "src/codex_cli_bin/bin/**",
            "src/codex_cli_bin/codex-resources/**",
            "src/codex_cli_bin/codex-path/**",
        ],
        "hooks": {"custom": {}},
    }
    assert pyproject["tool"]["hatch"]["build"]["targets"]["sdist"] == {
        "hooks": {"custom": {}},
    }
    assert sdist_guard is not None
    assert build_data_assignments == {
        "pure_python": False,
        "infer_tag": False,
        "tag": "joined-string",
    }
    assert 'WINDOWS_PLATFORM_TAGS = frozenset({"win_amd64", "win_arm64"})' in hook_source
    assert "unsupported wheel platform tag" in hook_source


def test_stage_runtime_release_copies_package_layout_and_sets_version(
    tmp_path: Path,
) -> None:
    script = _load_update_script_module()
    package_archive = _write_fake_codex_package_archive(tmp_path, script)

    staged = script.stage_python_runtime_package(
        tmp_path / "runtime-stage",
        "1.2.3",
        package_archive,
    )
    package_root = script.staged_runtime_package_root(staged)

    assert {
        "metadata": (package_root / "codex-package.json").read_text(),
        "codex": (package_root / "bin" / script.runtime_binary_name()).read_text(),
        "code_mode_host": (package_root / "bin" / script.runtime_code_mode_host_name()).read_text(),
        "rg": (package_root / "codex-path" / "rg").read_text(),
    } == {
        "metadata": '{"variant":"codex"}\n',
        "codex": "fake codex\n",
        "code_mode_host": "fake code mode host\n",
        "rg": "fake rg\n",
    }
    assert 'name = "openai-codex-cli-bin"' in (staged / "pyproject.toml").read_text()
    assert 'version = "1.2.3"' in (staged / "pyproject.toml").read_text()


def test_normalize_codex_version_accepts_release_tags_and_pep440_versions() -> None:
    script = _load_update_script_module()

    assert script.normalize_codex_version("rust-v0.116.0-alpha.1") == "0.116.0a1"
    assert script.normalize_codex_version("v0.116.0-beta.2") == "0.116.0b2"
    assert script.normalize_codex_version("0.116.0rc3") == "0.116.0rc3"
    assert script.normalize_codex_version("0.116.0") == "0.116.0"


def test_stage_runtime_release_replaces_existing_staging_dir(tmp_path: Path) -> None:
    script = _load_update_script_module()
    staging_dir = tmp_path / "runtime-stage"
    old_file = staging_dir / "stale.txt"
    old_file.parent.mkdir(parents=True)
    old_file.write_text("stale")
    package_archive = _write_fake_codex_package_archive(tmp_path, script)

    staged = script.stage_python_runtime_package(
        staging_dir,
        "1.2.3",
        package_archive,
    )

    assert staged == staging_dir
    assert not old_file.exists()
    package_root = script.staged_runtime_package_root(staged)
    assert (package_root / "bin" / script.runtime_binary_name()).read_text() == "fake codex\n"


def test_stage_runtime_release_can_pin_wheel_platform_tag(tmp_path: Path) -> None:
    script = _load_update_script_module()
    package_archive = _write_fake_codex_package_archive(tmp_path, script)

    staged = script.stage_python_runtime_package(
        tmp_path / "runtime-stage",
        "0.116.0a1",
        package_archive,
        platform_tag="win_amd64",
    )

    pyproject = (staged / "pyproject.toml").read_text()
    assert 'platform-tag = "win_amd64"' in pyproject


def test_stage_runtime_release_rejects_incomplete_package_layout(tmp_path: Path) -> None:
    script = _load_update_script_module()
    package_dir = tmp_path / "codex-package"
    (package_dir / "bin").mkdir(parents=True)
    package_archive = tmp_path / "codex-package.tar.gz"
    _write_package_archive(package_dir, package_archive)

    with pytest.raises(RuntimeError, match="Missing Codex package layout entries"):
        script.stage_python_runtime_package(tmp_path / "runtime-stage", "1.2.3", package_archive)


@pytest.mark.parametrize(
    ("member_name", "member_type", "link_name"),
    [
        ("../escaped.txt", tarfile.REGTYPE, None),
        ("/absolute.txt", tarfile.REGTYPE, None),
        ("linked", tarfile.SYMTYPE, "../escaped.txt"),
        ("hard-linked", tarfile.LNKTYPE, "../escaped.txt"),
        ("pipe", tarfile.FIFOTYPE, None),
        ("device", tarfile.CHRTYPE, None),
    ],
)
def test_runtime_archive_extraction_rejects_unsafe_members(
    tmp_path: Path,
    member_name: str,
    member_type: bytes,
    link_name: str | None,
) -> None:
    script = _load_update_script_module()
    archive_path = tmp_path / "unsafe.tar.gz"
    with tarfile.open(archive_path, "w:gz") as archive:
        member = tarfile.TarInfo(member_name)
        member.type = member_type
        if link_name is not None:
            member.linkname = link_name
            archive.addfile(member)
        else:
            payload = b"unsafe"
            member.size = len(payload)
            archive.addfile(member, io.BytesIO(payload))

    with pytest.raises(RuntimeError, match="Unsafe archive|escapes staging"):
        script._extract_codex_package_archive(archive_path, tmp_path / "stage")
    assert not (tmp_path / "escaped.txt").exists()


def test_runtime_package_layout_is_included_by_wheel_config(
    tmp_path: Path,
) -> None:
    script = _load_update_script_module()
    package_archive = _write_fake_codex_package_archive(tmp_path, script)

    staged = script.stage_python_runtime_package(
        tmp_path / "runtime-stage",
        "1.2.3",
        package_archive,
    )

    pyproject = tomllib.loads((staged / "pyproject.toml").read_text())
    assert pyproject["tool"]["hatch"]["build"]["targets"]["wheel"]["include"] == [
        "src/codex_cli_bin/codex-package.json",
        "src/codex_cli_bin/bin/**",
        "src/codex_cli_bin/codex-resources/**",
        "src/codex_cli_bin/codex-path/**",
    ]


def test_stage_sdk_release_pins_matching_runtime_version(tmp_path: Path) -> None:
    script = _load_update_script_module()
    staged = script.stage_python_sdk_package(
        tmp_path / "sdk-stage",
        "0.1.0b1",
        "rust-v1.2.3",
    )

    pyproject = tomllib.loads((staged / "pyproject.toml").read_text())
    assert {
        "name": pyproject["project"]["name"],
        "version": pyproject["project"]["version"],
        "dependencies": pyproject["project"]["dependencies"],
    } == {
        "name": "openai-codex",
        "version": "0.1.0b1",
        "dependencies": [
            "pydantic>=2.12",
            "openai-codex-cli-bin==1.2.3",
        ],
    }
    assert (
        '__version__ = "0.1.0b1"'
        not in (staged / "src" / "openai_codex" / "__init__.py").read_text()
    )
    assert (
        'client_version: str = "0.1.0b1"'
        not in (staged / "src" / "openai_codex" / "client.py").read_text()
    )
    assert not any((staged / "src" / "openai_codex").glob("bin/**"))


def test_stage_sdk_release_replaces_existing_staging_dir(tmp_path: Path) -> None:
    script = _load_update_script_module()
    staging_dir = tmp_path / "sdk-stage"
    old_file = staging_dir / "stale.txt"
    old_file.parent.mkdir(parents=True)
    old_file.write_text("stale")

    staged = script.stage_python_sdk_package(staging_dir, "0.1.0b1", "1.2.3")

    assert staged == staging_dir
    assert not old_file.exists()


def test_sdk_beta_release_can_pin_stable_runtime(tmp_path: Path) -> None:
    script = _load_update_script_module()
    package_archive = _write_fake_codex_package_archive(tmp_path, script)

    sdk_stage = script.stage_python_sdk_package(
        tmp_path / "sdk-stage",
        "0.1.0b1",
        "1.2.3",
    )
    runtime_stage = script.stage_python_runtime_package(
        tmp_path / "runtime-stage",
        "1.2.3",
        package_archive,
    )

    sdk_pyproject = tomllib.loads((sdk_stage / "pyproject.toml").read_text())
    runtime_pyproject = tomllib.loads((runtime_stage / "pyproject.toml").read_text())

    assert {
        "sdk_version": sdk_pyproject["project"]["version"],
        "runtime_version": runtime_pyproject["project"]["version"],
        "sdk_dependencies": sdk_pyproject["project"]["dependencies"],
    } == {
        "sdk_version": "0.1.0b1",
        "runtime_version": "1.2.3",
        "sdk_dependencies": [
            "pydantic>=2.12",
            "openai-codex-cli-bin==1.2.3",
        ],
    }


def test_stage_sdk_runs_type_generation_before_staging(tmp_path: Path) -> None:
    script = _load_update_script_module()
    calls: list[str] = []
    args = script.parse_args(
        [
            "stage-sdk",
            str(tmp_path / "sdk-stage"),
            "--sdk-version",
            "0.1.0b1",
            "--codex-version",
            "rust-v1.2.3",
        ]
    )

    def fake_generate_types() -> None:
        calls.append("generate_types")

    def fake_stage_sdk_package(_staging_dir: Path, sdk_version: str, runtime_version: str) -> Path:
        calls.append(f"stage_sdk:{sdk_version}:{runtime_version}")
        return tmp_path / "sdk-stage"

    def fake_stage_runtime_package(
        _staging_dir: Path,
        _runtime_version: str,
        _package_dir: Path,
        _platform_tag: str | None,
    ) -> Path:
        raise AssertionError("runtime staging should not run for stage-sdk")

    def fake_current_sdk_version() -> str:
        return "0.116.0a1"

    ops = script.CliOps(
        generate_types=fake_generate_types,
        stage_python_sdk_package=fake_stage_sdk_package,
        stage_python_runtime_package=fake_stage_runtime_package,
        current_sdk_version=fake_current_sdk_version,
    )

    script.run_command(args, ops)

    assert calls == ["generate_types", "stage_sdk:0.1.0b1:1.2.3"]


def test_stage_runtime_stages_package_without_type_generation(tmp_path: Path) -> None:
    script = _load_update_script_module()
    package_archive = _write_fake_codex_package_archive(tmp_path, script)
    calls: list[str] = []
    args = script.parse_args(
        [
            "stage-runtime",
            str(tmp_path / "runtime-stage"),
            str(package_archive),
            "--codex-version",
            "rust-v0.116.0-alpha.1",
            "--platform-tag",
            "win_amd64",
        ]
    )

    def fake_generate_types() -> None:
        calls.append("generate_types")

    def fake_stage_sdk_package(
        _staging_dir: Path,
        _sdk_version: str,
        _runtime_version: str,
    ) -> Path:
        raise AssertionError("sdk staging should not run for stage-runtime")

    def fake_stage_runtime_package(
        _staging_dir: Path,
        codex_version: str,
        package_archive: Path,
        platform_tag: str | None,
    ) -> Path:
        calls.append(f"stage_runtime:{codex_version}:{platform_tag}:{package_archive.name}")
        return tmp_path / "runtime-stage"

    def fake_current_sdk_version() -> str:
        return "0.116.0a1"

    ops = script.CliOps(
        generate_types=fake_generate_types,
        stage_python_sdk_package=fake_stage_sdk_package,
        stage_python_runtime_package=fake_stage_runtime_package,
        current_sdk_version=fake_current_sdk_version,
    )

    script.run_command(args, ops)

    assert calls == ["stage_runtime:0.116.0a1:win_amd64:codex-package.tar.gz"]


def test_default_runtime_is_resolved_from_installed_runtime_package(
    tmp_path: Path,
) -> None:
    from openai_codex import client as client_module

    fake_binary = tmp_path / "codex.exe"
    fake_binary.write_text("")
    ops = client_module.CodexBinResolverOps(
        installed_codex_path=lambda: fake_binary,
        path_exists=lambda path: path == fake_binary,
    )

    config = client_module.CodexConfig()
    assert config.codex_bin is None
    assert client_module.resolve_codex_bin(config, ops) == fake_binary


def test_runtime_path_dir_is_prepended_without_duplicates(tmp_path: Path) -> None:
    from openai_codex import client as client_module

    path_dir = tmp_path / "codex-path"
    env = {"Path": os.pathsep.join([r"C:\Tools", str(path_dir), r"C:\Windows"])}

    client_module._prepend_path_dirs(env, (path_dir,))

    assert env["Path"] == os.pathsep.join([str(path_dir), r"C:\Tools", r"C:\Windows"])


def test_runtime_path_dir_preserves_windows_path_key(
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    from openai_codex import client as client_module

    path_dir = tmp_path / "codex-path"
    monkeypatch.setattr(client_module.os, "name", "nt")
    env = {
        "PATH": r"C:\Tools",
        "Path": os.pathsep.join(["C\\Windows", str(path_dir)]),
    }

    client_module._prepend_path_dirs(env, (path_dir,))

    assert env == {"Path": os.pathsep.join([str(path_dir), "C\\Windows"])}


def test_explicit_codex_bin_override_takes_priority(tmp_path: Path) -> None:
    from openai_codex import client as client_module

    explicit_binary = tmp_path / "custom-codex.exe"
    explicit_binary.write_text("")
    ops = client_module.CodexBinResolverOps(
        installed_codex_path=lambda: (_ for _ in ()).throw(
            AssertionError("packaged runtime should not be used")
        ),
        path_exists=lambda path: path == explicit_binary,
    )

    config = client_module.CodexConfig(codex_bin=str(explicit_binary))
    assert client_module.resolve_codex_bin(config, ops) == explicit_binary


def test_missing_runtime_package_requires_explicit_codex_bin() -> None:
    from openai_codex import client as client_module

    ops = client_module.CodexBinResolverOps(
        installed_codex_path=lambda: (_ for _ in ()).throw(
            FileNotFoundError("missing packaged runtime")
        ),
        path_exists=lambda _path: False,
    )

    with pytest.raises(FileNotFoundError, match="missing packaged runtime"):
        client_module.resolve_codex_bin(client_module.CodexConfig(), ops)


def test_broken_runtime_package_does_not_fall_back() -> None:
    from openai_codex import client as client_module

    ops = client_module.CodexBinResolverOps(
        installed_codex_path=lambda: (_ for _ in ()).throw(
            FileNotFoundError("missing packaged binary")
        ),
        path_exists=lambda _path: False,
    )

    with pytest.raises(FileNotFoundError) as exc_info:
        client_module.resolve_codex_bin(client_module.CodexConfig(), ops)

    assert str(exc_info.value) == ("missing packaged binary")
