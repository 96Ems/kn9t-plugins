"""Hot reload: adding an MCP server to mcp.toml must reach the agent.

`Plugin._config_watcher_loop` polls `~/.kn9t/mcp.toml` and calls `_reload_config`,
which sends a `declare` message to kn9t-server (R-PLUG2-110). The server rebuilds
its tool registry and broadcasts `plugin_declared`, so new MCP tools become
callable without restarting the plugin.

This test drives the real plugin as a subprocess with a real (fake) stdio MCP
server, edits the config on disk, and waits for the `declare` to appear — no
mocking of the reload path itself.
"""

from __future__ import annotations

import json
import os
import queue
import subprocess
import sys
import threading
import time
from pathlib import Path

PLUGIN_ROOT = Path(__file__).parent.parent

# A minimal MCP server: answers initialize + tools/list over stdio. Tool specs
# arrive as a JSON array in argv[1] so one script serves any server.
FAKE_MCP_SERVER = '''
import json
import sys

tools = json.loads(sys.argv[1])

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    if "id" not in msg:  # notification
        continue
    if msg.get("method") == "initialize":
        result = {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "serverInfo": {"name": "fake", "version": "1.0"},
        }
    elif msg.get("method") == "tools/list":
        result = {"tools": tools}
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}) + "\\n")
    sys.stdout.flush()
'''


def _tool(name: str) -> dict:
    return {"name": name, "description": f"{name} description", "inputSchema": {"type": "object"}}


class _Plugin:
    """A running kn9t-mcp subprocess with a line reader on stdout."""

    def __init__(self, home: Path) -> None:
        env = os.environ.copy()
        env["HOME"] = str(home)          # POSIX
        env["USERPROFILE"] = str(home)   # Windows
        self.proc = subprocess.Popen(
            [sys.executable, "-m", "kn9t_mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
            cwd=PLUGIN_ROOT,
            env=env,
        )
        self.lines: "queue.Queue[dict]" = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self) -> None:
        assert self.proc.stdout is not None
        for line in self.proc.stdout:
            line = line.strip()
            if line:
                self.lines.put(json.loads(line))

    def send(self, msg: dict) -> None:
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def expect(self, kind: str, timeout: float = 15.0) -> dict:
        """Return the next message of `kind`, skipping others."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                msg = self.lines.get(timeout=deadline - time.monotonic())
            except queue.Empty:
                break
            if msg.get("t") == kind:
                return msg
        raise AssertionError(f"no '{kind}' message within {timeout}s")

    def close(self) -> None:
        try:
            self.send({"t": "shutdown"})
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()


def _write_config(home: Path, servers: list[tuple[str, str]]) -> Path:
    """Write mcp.toml with local servers, each running the fake MCP server."""
    home.mkdir(parents=True, exist_ok=True)
    fake = home / "fake_mcp_server.py"
    fake.write_text(FAKE_MCP_SERVER, encoding="utf-8")

    lines = []
    for name, tool_name in servers:
        tools_json = json.dumps([_tool(tool_name)])
        cmd = [sys.executable, str(fake), tools_json]
        lines += [
            "[[mcp]]",
            f'name = "{name}"',
            'type = "local"',
            f"cmd = {json.dumps(cmd)}",
            "",
        ]

    config = home / ".kn9t" / "mcp.toml"
    config.parent.mkdir(parents=True, exist_ok=True)
    config.write_text("\n".join(lines), encoding="utf-8")
    # Force a distinct mtime: the watcher compares timestamps, and a coarse
    # filesystem clock could otherwise make edit and initial read identical.
    future = time.time() + 5
    os.utime(config, (future, future))
    return config


def test_adding_a_server_hot_reloads_its_tools(tmp_path: Path):
    """A server added to mcp.toml after startup is declared without a restart."""
    home = tmp_path / "home"
    _write_config(home, [("alpha", "alpha_tool")])

    plugin = _Plugin(home)
    try:
        plugin.send({"t": "hello", "proto": 1, "kn9t": "0.1.0"})
        hello = plugin.expect("hello")

        initial_tools = {t["name"] for t in hello["tools"]}
        assert "mcp_alpha_alpha_tool" in initial_tools
        assert "mcp_beta_beta_tool" not in initial_tools

        # Add a second server to the config, as a user editing mcp.toml would.
        _write_config(home, [("alpha", "alpha_tool"), ("beta", "beta_tool")])

        declare = plugin.expect("declare")
        declared = {t["name"] for t in declare["tools"]}

        assert "mcp_beta_beta_tool" in declared, "new server's tools must be declared"
        assert "mcp_alpha_alpha_tool" in declared, "declare carries the full tool list"
    finally:
        plugin.close()


def test_editing_a_server_reconnects_it(tmp_path: Path):
    """Changing an existing server's definition reconnects without a restart.

    A live connection cannot see an edited `cmd`, `env`, `url` or header — so an
    edited entry must be torn down and rebuilt. This is the case that bit a
    `[mcp.headers] User-Agent` added to an already-connected server.
    """
    home = tmp_path / "home"
    _write_config(home, [("alpha", "alpha_tool")])

    plugin = _Plugin(home)
    try:
        plugin.send({"t": "hello", "proto": 1, "kn9t": "0.1.0"})
        hello = plugin.expect("hello")
        assert "mcp_alpha_alpha_tool" in {t["name"] for t in hello["tools"]}

        # Rewrite the same server with a different command (a stand-in for any
        # definition change: cmd, env, url or headers).
        _write_config(home, [("alpha", "alpha_v2_tool")])

        declare = plugin.expect("declare")
        declared = {t["name"] for t in declare["tools"]}

        assert "mcp_alpha_alpha_v2_tool" in declared, "edited server must be reconnected"
        assert "mcp_alpha_alpha_tool" not in declared, "old tools must be gone"
    finally:
        plugin.close()


def test_removing_a_server_redeclares_without_its_tools(tmp_path: Path):
    """Removing a server drops its tools from the declaration."""
    home = tmp_path / "home"
    _write_config(home, [("alpha", "alpha_tool"), ("beta", "beta_tool")])

    plugin = _Plugin(home)
    try:
        plugin.send({"t": "hello", "proto": 1, "kn9t": "0.1.0"})
        hello = plugin.expect("hello")
        assert "mcp_beta_beta_tool" in {t["name"] for t in hello["tools"]}

        _write_config(home, [("alpha", "alpha_tool")])

        declare = plugin.expect("declare")
        declared = {t["name"] for t in declare["tools"]}

        assert "mcp_beta_beta_tool" not in declared, "removed server's tools must be gone"
        assert "mcp_alpha_alpha_tool" in declared
    finally:
        plugin.close()
