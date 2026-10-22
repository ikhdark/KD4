"""Warm SQLx build-input regression using the actual state build script."""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest

from scripts.build.build_tooling_test_support import REPO_ROOT, load_toml
from scripts.build.rust_tool_env import local_rust_env


class StateMigrationInputsTest(unittest.TestCase):
    def test_sql_only_changes_refresh_each_embedded_migration_set(self):
        cargo = shutil.which("cargo")
        if cargo is None:
            self.skipTest("Cargo is required")
        state = REPO_ROOT / "codex-rs/state"
        directories = (
            "migrations",
            "logs_migrations",
            "goals_migrations",
            "bugs_migrations",
        )
        declarations = (state / "src/migrations.rs").read_text(encoding="utf-8")
        self.assertEqual(
            set(re.findall(r'sqlx::migrate!\("\./([^"]+)"\)', declarations)),
            set(directories),
        )
        toolchain = load_toml(REPO_ROOT / "codex-rs/rust-toolchain.toml")["toolchain"][
            "channel"
        ]
        with tempfile.TemporaryDirectory(prefix="state-migration-inputs-") as temporary:
            root = Path(temporary)
            (root / "src").mkdir()
            shutil.copyfile(state / "build.rs", root / "build.rs")
            (root / "Cargo.toml").write_text(
                '[package]\nname = "state-migration-input-probe"\nversion = "0.1.0"\nedition = "2024"\n'
                "[workspace]\n[dependencies]\n"
                'sqlx = { version = "=0.9.0", default-features = false, '
                'features = ["macros", "migrate", "runtime-tokio", "sqlite-bundled"] }\n',
                encoding="utf-8",
            )
            sets = ",\n".join(
                f'("{directory}", sqlx::migrate!("./{directory}"))'
                for directory in directories
            )
            (root / "src/main.rs").write_text(
                "fn main() { for (directory, migrator) in [" + sets + "] {\n"
                "for migration in migrator.iter() {\n"
                'println!("{}|{}|{}", directory, migration.version, migration.sql.as_str().trim());\n'
                "} } }\n",
                encoding="utf-8",
            )
            for directory in directories:
                (root / directory).mkdir()
                (root / directory / "0001_initial.sql").write_text(
                    "SELECT 1;\n", encoding="utf-8"
                )
            env = os.environ.copy()
            env.update(local_rust_env(env, repo_root=REPO_ROOT))
            env.update(
                RUSTUP_TOOLCHAIN=toolchain, CARGO_TARGET_DIR=str(root / "target")
            )

            def build_and_observe():
                build = subprocess.run(
                    [cargo, "build", "--offline", "--message-format=json"],
                    cwd=root,
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=600,
                )
                self.assertEqual(build.returncode, 0, build.stdout + build.stderr)
                artifact = next(
                    event
                    for line in build.stdout.splitlines()
                    if (event := json.loads(line)).get("reason") == "compiler-artifact"
                    and event["target"]["name"] == "state-migration-input-probe"
                    and event.get("executable")
                )
                observed = subprocess.run(
                    [artifact["executable"]],
                    cwd=root,
                    env=env,
                    capture_output=True,
                    text=True,
                    timeout=30,
                )
                self.assertEqual(observed.returncode, 0, observed.stderr)
                # Independent disk input, never the count of the compiled migration set.
                expected = sorted(
                    f"{directory}|{int(path.name.split('_', 1)[0])}|{path.read_text(encoding='utf-8').strip()}"
                    for directory in directories
                    for path in (root / directory).glob("*.sql")
                )
                self.assertEqual(sorted(observed.stdout.splitlines()), expected)
                return artifact["fresh"]

            self.assertFalse(build_and_observe())
            self.assertTrue(build_and_observe())
            for directory in directories:
                with self.subTest(directory=directory):
                    added = root / directory / "0002_added.sql"
                    added.write_text("SELECT 2;\n", encoding="utf-8")
                    self.assertFalse(build_and_observe())
                    initial = root / directory / "0001_initial.sql"
                    initial.write_text("SELECT 3;\n", encoding="utf-8")
                    self.assertFalse(build_and_observe())
                    renamed = added.with_name("0003_renamed.sql")
                    added.rename(renamed)
                    self.assertFalse(build_and_observe())
                    renamed.unlink()
                    self.assertFalse(build_and_observe())
            self.assertTrue(build_and_observe())
