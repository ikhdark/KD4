"""Offline tests; native fork smoke test uses only a loopback scripted model."""

import asyncio
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
import zipfile
from contextlib import redirect_stdout
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

from harbor.models.agent.context import AgentContext

from scripts import benchmark_agents as launcher
from scripts.harbor_windows_codex import (
    AppServer,
    SandboxTerminal,
    WindowsCodex,
    WindowsPatchReplay,
    capture_patch,
    find_patch,
    validate_binary,
)


class FakeEnvironment:
    def __init__(self):
        self.calls = []
        self.uploads = []
        self.rc = 0

    async def exec(self, **kwargs):
        self.calls.append(kwargs)
        command = kwargs["command"]
        if command == "git rev-parse --verify HEAD":
            return SimpleNamespace(return_code=0, stderr="", stdout="1" * 40 + "\n")
        return SimpleNamespace(
            return_code=self.rc, stderr="", stdout="/app\n" if command.endswith("pwd") else "sandbox-marker\n",
        )

    async def download_file(self, source, target):
        Path(target).write_bytes(b"diff --git a/a b/a\n")

    async def upload_file(self, source, target):
        self.uploads.append((source, target))


class TerminalTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.logs = Path(self.temp.name)
        self.environment = FakeEnvironment()
        self.terminal = SandboxTerminal(self.environment, self.logs, "/app")

    async def test_command_uses_only_remote_environment_and_retains_exit(self):
        self.environment.rc = 7
        result = await self.terminal.execute("sandbox_terminal", {"command": "printf 'héllo'", "cwd": "/app/a b", "timeout_sec": 9})
        call = self.environment.calls[0]
        self.assertEqual(call["cwd"], "/app/a b")
        self.assertIn("setsid timeout -k 5s 9s bash -lc", call["command"])
        self.assertNotIn("env", call)
        self.assertEqual(result["return_code"], 7)
        self.assertEqual(len(list(self.logs.glob("terminal-*.json"))), 1)
        self.assertFalse(self.terminal.active)

    async def test_invalid_arguments_never_reach_environment(self):
        for args in ({"command": "echo x", "cwd": "C:\\host"}, {"command": "x", "timeout_sec": True},
                     {"command": "x", "timeout_sec": 301}, {"command": "x", "env": {"SECRET": "x"}},
                     {"command": "x\0"}, {"command": 1}):
            with self.subTest(args=args), self.assertRaises(ValueError):
                await self.terminal.execute("sandbox_terminal", args)
        with self.assertRaises(ValueError):
            await self.terminal.execute("host_shell", {"command": "x"})
        self.assertFalse(self.environment.calls)

    async def test_cancelled_remote_command_is_stopped_without_retry(self):
        async def cancel(**kwargs):
            self.environment.calls.append(kwargs)
            raise asyncio.CancelledError()
        with patch.object(self.environment, "exec", side_effect=cancel), self.assertRaises(asyncio.CancelledError):
            await self.terminal.execute("sandbox_terminal", {"command": "sleep 90"})
        self.assertEqual(len(self.terminal.active), 1)
        await self.terminal.stop()
        self.assertEqual(len(self.environment.calls), 2)
        self.assertIn("kill -KILL", self.environment.calls[-1]["command"])
        self.assertFalse(self.terminal.active)

    async def test_patch_capture_uses_separate_index_and_binary_download(self):
        target = self.logs / "model.patch"
        await capture_patch(self.environment, "/app", target, "1" * 40)
        command = self.environment.calls[0]["command"]
        self.assertIn("GIT_INDEX_FILE=/tmp/", command)
        self.assertIn("--binary " + "1" * 40, command)
        self.assertNotIn("HEAD", command)
        self.assertNotIn("git reset", command)
        self.assertEqual(target.read_bytes(), b"diff --git a/a b/a\n")

    async def test_patch_failure_is_not_recorded_as_empty_success(self):
        self.environment.rc = 1
        target = self.logs / "model.patch"
        with self.assertRaises(RuntimeError):
            await capture_patch(self.environment, "/app", target, "1" * 40)
        self.assertFalse(target.exists())

    async def test_unconfirmed_cancellation_prevents_grading(self):
        self.terminal.active.add(self.terminal.control + "/pending.pid")
        self.environment.rc = 1
        with self.assertRaisesRegex(RuntimeError, "do not grade"):
            await self.terminal.stop()
        self.assertTrue(self.terminal.active)
        self.assertIn(".cancel", self.environment.calls[0]["command"])
        self.assertIn(".done", self.environment.calls[0]["command"])

    @unittest.skipUnless(os.environ.get("CODEX_BENCHMARK_TEST_BASH"), "Set CODEX_BENCHMARK_TEST_BASH for shell syntax check")
    async def test_generated_shell_syntax(self):
        await self.terminal.setup()
        await self.terminal.execute("sandbox_terminal", {"command": "printf '%s' 'quoted value'"})
        self.terminal.active.add(self.terminal.control + "/pending.pid")
        await self.terminal.stop()
        await capture_patch(self.environment, "/app", self.logs / "model.patch", "1" * 40)
        for call in self.environment.calls:
            result = subprocess.run([os.environ["CODEX_BENCHMARK_TEST_BASH"], "-n"], input=call["command"],
                                    text=True, capture_output=True, timeout=15, check=False)
            self.assertEqual(result.returncode, 0, result.stderr)

    async def test_replay_empty_patch_and_failed_apply(self):
        source = self.logs / "source"
        (source / "trial" / "agent").mkdir(parents=True)
        (source / "trial" / "result.json").write_text(json.dumps({"task_name": "instance_test"}))
        patch_path = source / "trial" / "agent" / "model.patch"
        patch_path.write_bytes(b"")
        trial = self.logs / "replay"
        trial.mkdir()
        (trial / "config.json").write_text(json.dumps({"task": {"path": "C:\\cache\\instance_test"}}))
        agent = WindowsPatchReplay(logs_dir=trial / "agent", source_job=str(source))
        await agent.run("", self.environment, AgentContext())
        self.assertFalse(self.environment.uploads)
        patch_path.write_bytes(b"not empty")
        self.environment.rc = 1
        with self.assertRaises(RuntimeError):
            await agent.run("", self.environment, AgentContext())
        self.assertIn("git apply --check", self.environment.calls[-1]["command"])


class ProcessStub:
    def __init__(self, frames):
        self.stdout = asyncio.StreamReader()
        for frame in frames:
            self.stdout.feed_data((json.dumps(frame) + "\n").encode())
        self.stdout.feed_eof()
        self.stdin = SimpleNamespace(write=lambda data: self.sent.append(json.loads(data)), drain=AsyncMock())
        self.sent = []
        self.returncode = None


class RpcTests(unittest.IsolatedAsyncioTestCase):
    async def test_dynamic_request_and_usage(self):
        frames = [
            {"id": 9, "method": "item/tool/call", "params": {"callId": "call", "tool": "sandbox_terminal", "arguments": {"command": "echo x"}}},
            {"method": "thread/tokenUsage/updated", "params": {"tokenUsage": {"total": {"inputTokens": 10, "cachedInputTokens": 3, "outputTokens": 2}}}},
        ]
        proc = ProcessStub(frames)
        terminal = SimpleNamespace(execute=AsyncMock(return_value={"return_code": 1}))
        context = AgentContext()
        rpc = AppServer(proc, terminal, io.StringIO(), context)
        await rpc.receive()
        await rpc.receive()
        self.assertTrue(proc.sent[0]["result"]["success"])
        self.assertEqual(proc.sent[0]["result"]["contentItems"][0]["type"], "inputText")
        self.assertEqual(context.n_cache_tokens, 3)

    async def test_duplicate_and_unexpected_requests_fail_closed(self):
        frame = {"id": 9, "method": "item/tool/call", "params": {"callId": "call", "tool": "sandbox_terminal", "arguments": {"command": "x"}}}
        proc = ProcessStub([frame, frame])
        terminal = SimpleNamespace(execute=AsyncMock(return_value={}))
        rpc = AppServer(proc, terminal, io.StringIO(), AgentContext())
        await rpc.receive()
        with self.assertRaisesRegex(RuntimeError, "Duplicate"):
            await rpc.receive()
        self.assertEqual(terminal.execute.await_count, 1)
        rpc = AppServer(ProcessStub([{"id": 3, "method": "approval/request"}]), terminal, io.StringIO(), AgentContext())
        with self.assertRaisesRegex(RuntimeError, "Unexpected"):
            await rpc.receive()

    async def test_eof_is_failure(self):
        rpc = AppServer(ProcessStub([]), None, io.StringIO(), AgentContext())
        with self.assertRaisesRegex(RuntimeError, "closed stdout"):
            await rpc.receive()


class LaunchTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        (self.home / "bin").mkdir()
        (self.home / "bin" / "codex.exe").write_bytes(b"MZfake")
        (self.home / "bin" / "codex-code-mode-host.exe").write_bytes(b"MZfake")
        (self.home / "config.toml").write_text('model = "explicit-model"\nmodel_reasoning_effort = "xhigh"\n')
        self.tasks = self.home / "benchmarks" / f"swe-bench-pro-v2-{launcher.SWE_REVISION[:12]}" / "v2" / "tasks"
        self.tasks.mkdir(parents=True)

    def args(self, *args):
        return launcher.parser().parse_args([*args, "--fork-home", str(self.home)])

    def test_terminal_pin_private_defaults_and_no_auto_execute(self):
        args = self.args("terminal")
        command, _ = launcher.build_command(args)
        self.assertFalse(args.execute)
        self.assertIn("terminal-bench/terminal-bench@4.0.0", command)
        self.assertIn("scripts.harbor_windows_codex:WindowsCodex", command)
        self.assertIn("reasoning_effort=xhigh", command)
        self.assertEqual(command[command.index("--n-tasks") + 1], "1")
        self.assertNotIn("--launch", command)
        self.assertNotIn("--ae", command)

    def test_swe_never_verifies_agent_sandbox(self):
        command, _ = launcher.build_command(self.args("swe"))
        self.assertIn("--disable-verification", command)
        self.assertIn("capture_patch=true", command)
        self.assertNotIn("--allow-agent-host", command)

    def test_dry_run_never_invokes_harbor_or_a_model(self):
        with patch.object(launcher, "harbor_python", return_value=Path(sys.executable)), \
                patch.object(launcher.subprocess, "run") as run, \
                patch.object(launcher.subprocess, "call") as call, redirect_stdout(io.StringIO()):
            self.assertEqual(launcher.main(["terminal", "--fork-home", str(self.home)]), 0)
        run.assert_not_called()
        call.assert_not_called()

    def test_swe_cache_corruption_rejected(self):
        root = self.tasks.parent.parent
        task_file = self.tasks / "task.txt"
        task_file.write_bytes(b"original")
        checksums = f"{launcher.sha256(task_file)}  tasks/task.txt\n".encode()
        (root / "v2" / "SHA256SUMS").write_bytes(checksums)
        archive = root / "source.zip"
        with zipfile.ZipFile(archive, "w") as source:
            source.writestr(f"SWE-bench_Pro-os-{launcher.SWE_REVISION}/v2/SHA256SUMS", checksums)
        with patch.object(launcher, "SWE_ARCHIVE_SHA256", launcher.sha256(archive)):
            self.assertEqual(launcher.verify_swe(root), 1)
            task_file.write_bytes(b"modified")
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                launcher.verify_swe(root)

    def test_harbor_factory_registers_the_windows_adapter(self):
        from harbor.agents.factory import AgentFactory

        agent = AgentFactory.create_agent_from_import_path(
            "scripts.harbor_windows_codex:WindowsCodex", logs_dir=self.home / "logs",
            model_name="explicit-model", codex_binary=str(self.home / "bin" / "codex.exe"), auth_home=str(self.home),
        )
        self.assertIsInstance(agent, WindowsCodex)

    def test_regrade_exact_source_tasks_and_ambiguity(self):
        source = self.home / "source"
        for name in ("instance_a", "instance_b"):
            task = self.tasks / name
            task.mkdir()
            (task / "task.toml").write_text("")
            trial = source / (name + "__trial")
            (trial / "agent").mkdir(parents=True)
            (trial / "result.json").write_text(json.dumps({"task_name": name}))
            (trial / "agent" / "model.patch").write_bytes(b"")
        args = self.args("regrade", "--source-job", str(source))
        command, _ = launcher.build_command(args)
        self.assertNotIn("--n-tasks", command)
        self.assertNotIn("--disable-verification", command)
        self.assertEqual(command.count("-i"), 2)
        resolved = subprocess.run([sys.executable, "-c", "from harbor.cli.main import app; app()", *command, "--print-config"],
                                  cwd=launcher.REPO_ROOT, capture_output=True, text=True, encoding="utf-8", timeout=30, check=False)
        self.assertEqual(resolved.returncode, 0, resolved.stderr)
        config = json.loads(resolved.stdout)
        self.assertEqual(config["agents"][0]["name"], "scripts.harbor_windows_codex:WindowsPatchReplay")
        self.assertFalse(config.get("verifier", {}).get("disable", False))
        self.assertEqual(find_patch(source, "instance_a").read_bytes(), b"")
        (source / "instance_a__trial" / "agent" / "model.patch").unlink()
        with self.assertRaisesRegex(ValueError, "missing"):
            launcher.build_command(args)
        with self.assertRaises(ValueError):
            find_patch(source, "instance_a")

    def test_existing_job_and_non_executable_are_rejected(self):
        job = self.home / "benchmarks" / "jobs" / "existing"
        job.mkdir(parents=True)
        with self.assertRaises(ValueError):
            launcher.build_command(self.args("terminal", "--job-name", "existing"))
        binary = self.home / "bin" / "codex.exe"
        binary.write_bytes(b"\x7fELF")
        with self.assertRaises(ValueError):
            validate_binary(binary)

    def test_extra_env_rejected_before_harbor_can_forward_credentials(self):
        with self.assertRaisesRegex(ValueError, "Do not use --ae"):
            WindowsCodex.preflight({"codex_binary": str(self.home / "bin" / "codex.exe"), "auth_home": str(self.home)}, {"OPENAI_API_KEY": "test-secret"})


@unittest.skipUnless(os.name == "nt" and os.environ.get("CODEX_BENCHMARK_TEST_BINARY"), "Set CODEX_BENCHMARK_TEST_BINARY for native offline smoke")
class NativeSmokeTest(unittest.IsolatedAsyncioTestCase):
    async def test_real_windows_fork_routes_dynamic_tool_to_harbor_without_host_tools(self):
        await self.smoke(code_mode=False)

    async def test_real_windows_fork_routes_code_mode_to_harbor(self):
        await self.smoke(code_mode=True)

    async def smoke(self, *, code_mode):
        import websockets

        from scripts.mock_responses_websocket_server import (
            _event_assistant_message,
            _event_function_call,
            _event_response_completed,
            _event_response_created,
        )
        requests = []
        failures = []

        async def model(socket):
            try:
                turn = 0
                async for raw in socket:
                    request = json.loads(raw)
                    requests.append(request)
                    response_id = f"smoke-{len(requests)}"
                    events = [_event_response_created(response_id)]
                    if request.get("generate") is not False:
                        turn += 1
                        if turn == 1:
                            if code_mode:
                                probe = (
                                    'const forbidden = ["exec_command","shell_command","apply_patch","spawn_agent"]; '
                                    'if (ALL_TOOL_NAMES.some(n => forbidden.includes(n))) throw new Error("host tools exposed"); '
                                    'let hostReadDenied = false; '
                                    f'try {{ await tools.read_file({{path:{json.dumps(str(auth / "config.toml"))}}}); }} '
                                    'catch (error) { hostReadDenied = String(error).includes("selected execution environment"); } '
                                    'if (!hostReadDenied) throw new Error("host filesystem read was not denied"); '
                                    'text(await tools.sandbox_terminal({command:"echo sandbox-marker"}));'
                                )
                                events.append({"type": "response.output_item.done", "item": {
                                    "type": "custom_tool_call", "call_id": "remote-call", "name": "exec",
                                    "input": probe,
                                }})
                            else:
                                events.append(_event_function_call("remote-call", "sandbox_terminal", json.dumps({"command": "echo sandbox-marker"})))
                        else:
                            events.append(_event_assistant_message("done", "Finished offline smoke."))
                    events.append(_event_response_completed(response_id))
                    for event in events:
                        await socket.send(json.dumps(event))
            except websockets.exceptions.ConnectionClosed:
                pass  # app-server closes its model sockets when stdin closes.
            except (ValueError, TypeError, KeyError) as exc:
                failures.append(str(exc))

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            auth = root / "source-home"
            auth.mkdir()
            # A dummy file meets preflight; the scripted provider never uses it.
            (auth / "auth.json").write_text('{"OPENAI_API_KEY":"offline-placeholder"}')
            (auth / "config.toml").write_text('developer_instructions="DO_NOT_INHERIT_USER_CONFIG"')
            environment = FakeEnvironment()
            agent = WindowsCodex(
                logs_dir=root / "logs", model_name="gpt-5.2", codex_binary=os.environ["CODEX_BENCHMARK_TEST_BINARY"],
                auth_home=str(auth), budget_sec=60, capture_patch=True,
            )
            context = AgentContext()
            real_spawn = asyncio.create_subprocess_exec
            spawned_homes = []
            async with websockets.serve(model, "127.0.0.1", 0) as server:
                port = server.sockets[0].getsockname()[1]

                async def spawn(*args, **kwargs):
                    if "app-server" in args:
                        spawned_homes.append(Path(kwargs["env"]["CODEX_HOME"]))
                        self.assertEqual(kwargs["env"]["CODEX_EXEC_SERVER_URL"], "none")
                        kwargs["env"]["OPENAI_API_KEY"] = "offline-placeholder"
                        kwargs["env"]["CODEX_API_KEY"] = "offline-placeholder"
                        args = [*args, "-c", 'model_provider="offline"',
                                "-c", f'model_providers.offline={{name="offline",base_url="http://127.0.0.1:{port}/v1",wire_api="responses",supports_websockets=true}}',
                                "-c", f"features.code_mode={str(code_mode).lower()}",
                                "-c", f"features.code_mode_only={str(code_mode).lower()}"]
                    return await real_spawn(*args, **kwargs)

                with patch("asyncio.create_subprocess_exec", side_effect=spawn):
                    try:
                        await asyncio.wait_for(agent.run("Use sandbox_terminal to echo sandbox-marker.", environment, context), 90)
                    except Exception:
                        for path in (root / "logs").glob("app-server*"):
                            print(path.name, path.read_text(encoding="utf-8")[-16000:])
                        raise
            self.assertFalse(failures)
            self.assertTrue(requests)
            all_text = json.dumps(requests)
            self.assertNotIn("DO_NOT_INHERIT_USER_CONFIG", all_text)
            def tool_names(specs):
                return {s["name"] for s in specs if "name" in s} | {
                    name for s in specs for name in tool_names(s.get("tools", []))
                }
            names = tool_names(requests[0].get("tools", []))
            self.assertFalse(names & {"exec_command", "shell_command", "apply_patch", "spawn_agent", "web_search"})
            self.assertIn("exec" if code_mode else "sandbox_terminal", names)
            result_type = "custom_tool_call_output" if code_mode else "function_call_output"
            self.assertTrue(any("sandbox-marker" in json.dumps(r.get("input")) and result_type in json.dumps(r.get("input")) for r in requests[1:]))
            self.assertEqual(sum("setsid timeout" in c["command"] for c in environment.calls), 1)
            self.assertTrue((root / "logs" / "model.patch").is_file())
            self.assertEqual((auth / "auth.json").read_text(), '{"OPENAI_API_KEY":"offline-placeholder"}')
            self.assertTrue(all(not p.exists() for p in spawned_homes))


if __name__ == "__main__":
    unittest.main()
