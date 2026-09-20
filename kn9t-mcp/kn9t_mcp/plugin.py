"""kn9t plugin v2 protocol implementation with lazy tool discovery.

This module implements the host-side of the kn9t plugin protocol,
allowing this Python plugin to communicate with kn9t-server.

**Lazy Tool Discovery:**
Instead of exposing all MCP tools (100+) in the hello, we expose only:
- `mcp_search_tools` — search and discover tools by server or query
- `mcp_list_servers` — list available MCP servers

When the agent calls `mcp_search_tools`, the matching tool specs are returned
in the tool result. The agent then sees them in context and can use them.
All tools remain registered internally but aren't in the initial tools array.

Protocol overview (stdin/stdout, newline-delimited JSON):

    Host -> Plugin:
        {"t": "hello", "proto": 1, "kn9t": "0.1.0"}
        {"t": "hook", "id": N, "hook": "tool_call", "payload": {...}}
        {"t": "cancel", "id": N}
        {"t": "shutdown"}

    Plugin -> Host:
        {"t": "hello", "name": "...", "capabilities": [...], "tools": [...]}
        {"t": "chunk", "id": N, "text": "..."}  # streaming progress
        {"t": "done", "id": N, "content": [...], "is_error": bool}  # Note: flat, not nested in body

Reference: kn9t/spec/08b-plugin-redesign.md
"""

from __future__ import annotations

import json
import sys
import threading
import time
from dataclasses import dataclass, field
from queue import Queue
from typing import Any

# TTL for discovered tools cache (5 minutes)
DISCOVERED_TTL_SECS = 300

# Config file watcher interval (seconds)
CONFIG_WATCH_INTERVAL_SECS = 2.0

from kn9t_mcp.config import McpServerConfig, load_mcp_config, mcp_config_mtime
from kn9t_mcp.mcp_client import McpClient, McpError, McpTool
from kn9t_mcp.mcp_http_client import McpHttpClient

# Union type for both client types
McpClientType = McpClient | McpHttpClient


@dataclass
class ToolSpec:
    """Internal representation of a tool exposed to kn9t."""

    name: str  # prefixed name: mcp_{server}_{tool}
    description: str
    schema: dict[str, Any]
    mcp_server: str  # which MCP server owns this tool
    mcp_tool_name: str  # original MCP tool name

    def to_spec_dict(self) -> dict[str, Any]:
        """Convert to JSON-serializable spec for discovery response."""
        return {
            "name": self.name,
            "description": self.description,
            "parameters": self.schema,  # OpenAI-style
        }


# ── Meta-tools for lazy discovery ─────────────────────────────────────────────

SEARCH_TOOLS_SPEC = {
    "name": "mcp_search_tools",
    "description": (
        "Discover MCP tools from a server. Returns tool specifications that you can "
        "then call DIRECTLY as tool_call (like read, edit, bash). "
        "Do NOT use bash to call these tools - they are native tools. "
        "Use mcp_list_servers first to see available servers. "
        "Be SPECIFIC with query to avoid too many results (e.g., 'create_issue' not 'issue'). "
        "Already-discovered tools are cached for 5 minutes and won't be returned again."
    ),
    "schema": {
        "type": "object",
        "properties": {
            "server": {
                "type": "string",
                "description": (
                    "MCP server name (e.g., 'teamforge', 'atlassian', 'testkub-nfc'). "
                    "Use mcp_list_servers to see available servers."
                ),
            },
            "query": {
                "type": "string",
                "description": (
                    "Filter tools by name or description. Be specific! "
                    "Examples: 'create_issue', 'get_artifact', 'search_page'. "
                    "Vague queries like 'issue' return too many results."
                ),
            },
            "limit": {
                "type": "integer",
                "description": "Max NEW tools to return (default 10, max 15).",
            },
        },
        "required": ["server"],
    },
    "parallel_safe": True,
}

LIST_SERVERS_SPEC = {
    "name": "mcp_list_servers",
    "description": (
        "List available MCP servers (TeamForge, Jira, Confluence, TestKub, etc.) "
        "and their tool counts. Call this first, then use mcp_search_tools to "
        "discover specific tools from a server."
    ),
    "schema": {
        "type": "object",
        "properties": {},
    },
    "parallel_safe": True,
}


class Plugin:
    """kn9t plugin that bridges to MCP servers.
    
    This plugin:
    1. Spawns local MCP servers or connects to remote ones
    2. Discovers tools from each server
    3. Handles kn9t tool_call hooks by routing to MCP servers
    4. Supports streaming progress and cancellation
    """

    def __init__(
        self,
        mcp_clients: dict[str, McpClientType],
        mcp_configs: dict[str, McpServerConfig] | None = None,
    ) -> None:
        """Initialize with MCP clients.
        
        Args:
            mcp_clients: Map of server name to connected client (local or remote).
            mcp_configs: Config each client was built from, keyed by name. Used on
                hot reload to detect that a server's definition changed.
        """
        self.mcp_clients = mcp_clients
        self.mcp_configs: dict[str, McpServerConfig] = mcp_configs or {}
        self.tools: dict[str, ToolSpec] = {}
        self.inflight: dict[int, threading.Event] = {}  # for cancellation
        self._stdin_lock = threading.Lock()
        self._stdout_lock = threading.Lock()
        
        # Cache of discovered tools: tool_name -> discovery_timestamp
        # Tools in this cache won't be returned again by mcp_search_tools
        self.discovered_cache: dict[str, float] = {}
        
        # File watcher state
        self._config_mtime: float | None = mcp_config_mtime()
        self._watcher_stop = threading.Event()
        self._watcher_thread: threading.Thread | None = None
        
        self._discover_all_tools()

    @staticmethod
    def _make_client(cfg: McpServerConfig) -> McpClientType:
        """Build a client for a server config (no handshake yet)."""
        if cfg.type == "local":
            return McpClient.spawn(cfg.name, cfg.cmd, cfg.env, cfg.timeout_ms)
        return McpHttpClient(
            name=cfg.name,
            url=cfg.url,
            headers=cfg.headers,
            timeout=cfg.timeout_ms / 1000.0,
        )

    @classmethod
    def from_config(cls) -> Plugin:
        """Load MCP server configs and connect to them.
        
        Returns:
            Plugin instance with all configured MCP servers connected.
        """
        configs = load_mcp_config()

        if not configs:
            print("No MCP servers configured, plugin will expose no tools", file=sys.stderr)
            return cls({})

        clients: dict[str, McpClientType] = {}
        configs_by_name: dict[str, McpServerConfig] = {}
        
        for cfg in configs:
            try:
                client = cls._make_client(cfg)
                
                # Handshake with MCP server (discover capabilities)
                client.discover()
                clients[cfg.name] = client
                configs_by_name[cfg.name] = cfg
                print(f"Connected to MCP server '{cfg.name}' ({cfg.type})", file=sys.stderr)
            except McpError as e:
                print(f"Failed to connect to MCP server '{cfg.name}': {e}", file=sys.stderr)
                # Continue with other servers
            except Exception as e:
                print(f"Unexpected error connecting to '{cfg.name}': {e}", file=sys.stderr)

        return cls(clients, configs_by_name)

    def _discover_all_tools(self) -> None:
        """Query all MCP servers for their tools."""
        for server_name, client in self.mcp_clients.items():
            try:
                mcp_tools = client.list_tools()
                print(
                    f"Discovered {len(mcp_tools)} tool(s) from '{server_name}'",
                    file=sys.stderr,
                )
                
                for tool in mcp_tools:
                    # Prefix tool names to avoid collisions across servers
                    prefixed_name = f"mcp_{server_name}_{tool.name}"
                    
                    self.tools[prefixed_name] = ToolSpec(
                        name=prefixed_name,
                        description=f"[{server_name}] {tool.description}",
                        schema=tool.input_schema,
                        mcp_server=server_name,
                        mcp_tool_name=tool.name,
                    )
            except McpError as e:
                print(f"Failed to list tools from '{server_name}': {e}", file=sys.stderr)

        print(f"Total tools available: {len(self.tools)}", file=sys.stderr)

    def run(self) -> None:
        """Main loop: read stdin, dispatch, write stdout.
        
        This method blocks until shutdown or stdin EOF.
        """
        # Handshake: read host hello
        hello = self._read_message()
        if hello is None or hello.get("t") != "hello":
            print("Invalid handshake from host", file=sys.stderr)
            return

        proto = hello.get("proto", 0)
        kn9t_version = hello.get("kn9t", "unknown")
        print(f"Connected to kn9t {kn9t_version} (proto {proto})", file=sys.stderr)

        # Send our hello with discovered tools
        self._send_hello()
        
        # Start config file watcher thread (R-PLUG2-110: hot reload via declare)
        self._start_config_watcher()

        # Main dispatch loop
        while True:
            msg = self._read_message()
            if msg is None:  # EOF
                print("stdin closed, shutting down", file=sys.stderr)
                break

            t = msg.get("t", "")
            
            if t == "shutdown":
                print("Received shutdown", file=sys.stderr)
                self._shutdown()
                break
            elif t == "hook":
                self._handle_hook(msg)
            elif t == "cancel":
                self._handle_cancel(msg)
            elif t == "event":
                # Fire-and-forget events, we don't handle any
                pass
            else:
                print(f"Unknown message type: {t}", file=sys.stderr)

    def _read_message(self) -> dict[str, Any] | None:
        """Read one JSON line from stdin (thread-safe)."""
        with self._stdin_lock:
            try:
                line = sys.stdin.readline()
            except Exception as e:
                print(f"Error reading stdin: {e}", file=sys.stderr)
                return None
                
        if not line:
            return None
            
        try:
            return json.loads(line)
        except json.JSONDecodeError as e:
            print(f"Invalid JSON from host: {e}", file=sys.stderr)
            return None

    def _write_message(self, msg: dict[str, Any]) -> None:
        """Write one JSON line to stdout (thread-safe)."""
        with self._stdout_lock:
            try:
                sys.stdout.write(json.dumps(msg, separators=(",", ":")) + "\n")
                sys.stdout.flush()
            except Exception as e:
                print(f"Error writing stdout: {e}", file=sys.stderr)

    def _send_hello(self) -> None:
        """Send plugin hello with all tools, meta-tools visible and MCP tools hidden.
        
        All tools are registered in kn9t's ToolRegistry (so they can be executed),
        but only meta-tools (hidden=false) are shown in the system prompt.
        MCP tools (hidden=true) are discovered via mcp_search_tools.
        """
        # Build server summary for description
        server_info = ", ".join(
            f"{name} ({len([t for t in self.tools.values() if t.mcp_server == name])} tools)"
            for name in self.mcp_clients.keys()
        )
        
        # Meta-tools: visible (hidden=false)
        list_servers = dict(LIST_SERVERS_SPEC)
        list_servers["description"] = (
            f"List available MCP servers: {server_info}. "
            "Use mcp_search_tools to discover tools from a specific server."
        )
        list_servers["hidden"] = False
        
        search_tools = dict(SEARCH_TOOLS_SPEC)
        search_tools["hidden"] = False
        
        # MCP tools: hidden (lazy discovery)
        mcp_tools = [
            {
                "name": t.name,
                "description": t.description,
                "schema": t.schema,
                "parallel_safe": True,
                "hidden": True,  # Not shown in system prompt
            }
            for t in self.tools.values()
        ]
        
        # All tools: meta-tools (visible) + MCP tools (hidden)
        all_tools = [list_servers, search_tools] + mcp_tools
        
        self._write_message({
            "t": "hello",
            "name": "kn9t-mcp",
            "capabilities": ["streaming", "cancelable"],
            "tools": all_tools,
        })
        
        print(
            f"Registered {len(all_tools)} tools: 2 visible meta-tools, {len(mcp_tools)} hidden MCP tools",
            file=sys.stderr,
        )

    def _handle_hook(self, msg: dict[str, Any]) -> None:
        """Dispatch hook invocation."""
        hook_id: int = msg.get("id", 0)
        hook_name: str = msg.get("hook", "")
        payload: dict[str, Any] = msg.get("payload", {})

        if hook_name != "tool_call":
            # We only handle tool_call hooks
            # Return "keep" for other hooks (pass-through)
            self._write_message({"t": "result", "id": hook_id, "action": "keep"})
            return

        # Tool call
        tool_name: str = payload.get("tool", "")
        args: dict[str, Any] = payload.get("args", {})

        # Accept meta-tools, known MCP tools, and dynamically discovered tools
        is_meta_tool = tool_name in ("mcp_list_servers", "mcp_search_tools")
        is_known_tool = tool_name in self.tools
        
        if not is_meta_tool and not is_known_tool:
            self._send_error(hook_id, f"Unknown tool: {tool_name}")
            return

        # Setup cancellation
        cancel_event = threading.Event()
        self.inflight[hook_id] = cancel_event

        # Execute in thread to not block stdin reading
        thread = threading.Thread(
            target=self._execute_tool,
            args=(hook_id, tool_name, args, cancel_event),
            daemon=True,
        )
        thread.start()

    def _execute_tool(
        self,
        hook_id: int,
        tool_name: str,
        args: dict[str, Any],
        cancel: threading.Event,
    ) -> None:
        """Execute a tool call (runs in worker thread)."""
        try:
            # Handle meta-tools for lazy discovery
            if tool_name == "mcp_list_servers":
                self._handle_list_servers(hook_id)
                return
            
            if tool_name == "mcp_search_tools":
                self._handle_search_tools(hook_id, args)
                return
            
            # Regular MCP tool call
            if tool_name not in self.tools:
                self._send_error(hook_id, f"Unknown tool: {tool_name}")
                return
                
            spec = self.tools[tool_name]
            client = self.mcp_clients[spec.mcp_server]

            # Send progress update
            self._write_message({
                "t": "chunk",
                "id": hook_id,
                "text": f"Calling {spec.mcp_server}/{spec.mcp_tool_name}...",
            })

            # Check for cancellation
            if cancel.is_set():
                self._send_cancelled(hook_id)
                return

            # Call MCP server
            result = client.call_tool(spec.mcp_tool_name, args, cancel)

            # Check for cancellation again
            if cancel.is_set():
                self._send_cancelled(hook_id)
                return

            # Format result for better TUI display
            formatted = self._format_mcp_result(result)
            self._send_success(hook_id, formatted)

        except McpError as e:
            self._write_message({
                "t": "done",
                "id": hook_id,
                "content": [{"type": "text", "text": f"MCP error: {e}"}],
                "is_error": True,
            })
        except Exception as e:
            self._write_message({
                "t": "done",
                "id": hook_id,
                "content": [{"type": "text", "text": f"Unexpected error: {e}"}],
                "is_error": True,
            })
        finally:
            self.inflight.pop(hook_id, None)

    def _handle_list_servers(self, hook_id: int) -> None:
        """Handle mcp_list_servers: return available servers and tool counts."""
        # Build server list
        lines = ["MCP Servers Available:", ""]
        
        for name in sorted(self.mcp_clients.keys()):
            tool_count = len([t for t in self.tools.values() if t.mcp_server == name])
            lines.append(f"  • {name}: {tool_count} tools")
            # Stream each server as progress
            self._write_message({
                "t": "chunk",
                "id": hook_id,
                "text": f"  • {name}: {tool_count} tools",
            })
        
        lines.append("")
        lines.append(f"Total: {len(self.tools)} tools across {len(self.mcp_clients)} servers")
        lines.append("")
        lines.append("Next: call mcp_search_tools(server='<name>') to discover tools from a server.")
        
        self._send_success(hook_id, "\n".join(lines))

    def _handle_search_tools(self, hook_id: int, args: dict[str, Any]) -> None:
        """Handle mcp_search_tools: return tool specs for a server.
        
        Returns tool specifications that the agent can use immediately.
        The specs are returned in the tool result, so they appear in context.
        Tools already discovered (in cache with valid TTL) are excluded.
        """
        server = args.get("server", "")
        query = args.get("query", "").lower()
        limit = min(args.get("limit", 10), 15)  # Default 10, max 15
        
        if not server:
            self._send_error(hook_id, "server parameter is required. Use mcp_list_servers() first.")
            return
        
        # Check if server exists
        if server not in self.mcp_clients:
            available = sorted(self.mcp_clients.keys())
            self._send_error(hook_id, f"Unknown server: {server}. Available: {', '.join(available)}")
            return
        
        # Purge expired entries from discovered cache
        now = time.time()
        expired = [k for k, ts in self.discovered_cache.items() if now - ts > DISCOVERED_TTL_SECS]
        for k in expired:
            del self.discovered_cache[k]
        
        # Progress: searching
        self._write_message({
            "t": "chunk",
            "id": hook_id,
            "text": f"Searching {server} tools" + (f" for '{query}'..." if query else "..."),
        })
        
        # Filter tools by server
        matching_tools = [
            t for t in self.tools.values()
            if t.mcp_server == server
        ]
        
        # Further filter by query if provided
        if query:
            matching_tools = [
                t for t in matching_tools
                if query in t.name.lower() or query in t.description.lower()
            ]
        
        # Exclude already-discovered tools (still in cache)
        already_discovered = [t for t in matching_tools if t.name in self.discovered_cache]
        matching_tools = [t for t in matching_tools if t.name not in self.discovered_cache]
        
        if not matching_tools:
            if already_discovered:
                msg = f"All {len(already_discovered)} matching tools were already discovered. Try a different query."
            elif query:
                msg = f"No tools found matching '{query}'. Try a more specific query."
            else:
                msg = f"No tools in {server}"
            self._send_success(hook_id, msg)
            return
        
        total_matching = len(matching_tools)
        truncated = total_matching > limit
        matching_tools = matching_tools[:limit]
        
        # Stream each tool name as progress (for TUI display)
        tool_specs = []
        for t in matching_tools:
            self._write_message({
                "t": "chunk",
                "id": hook_id,
                "text": f"• {t.name}",
            })
            tool_specs.append(t.to_spec_dict())
        
        # Build final output - summary + JSON specs only (no duplication)
        if truncated:
            header = f"Found {total_matching} matching '{query}' (showing {limit}). Use a more specific query."
        else:
            header = f"Found {len(matching_tools)} tool(s)" + (f" matching '{query}'" if query else "") + "."
        
        lines = [
            header,
            "",
            "Call these tools directly as tool_call (like read/edit/bash), NOT via bash.",
            "",
            json.dumps(tool_specs, indent=2),
        ]
        
        # Mark these tools as discovered (add to cache with current timestamp)
        now = time.time()
        for t in matching_tools:
            self.discovered_cache[t.name] = now
        
        self._send_success(hook_id, "\n".join(lines))

    def _send_success(self, hook_id: int, result: str) -> None:
        """Send success response.
        
        Note: content and is_error are at root level (not nested in body)
        because Rust uses #[serde(flatten)] which expects flat structure.
        """
        self._write_message({
            "t": "done",
            "id": hook_id,
            "content": [{"type": "text", "text": result}],
            "is_error": False,
        })

    def _format_mcp_result(self, result: str) -> str:
        """Format MCP result for better TUI display.
        
        If the result is JSON, pretty-print it. Otherwise return as-is.
        """
        # Try to parse and pretty-print JSON
        try:
            parsed = json.loads(result)
            return json.dumps(parsed, indent=2, ensure_ascii=False)
        except (json.JSONDecodeError, TypeError):
            # Not JSON, return as-is
            return result

    def _handle_cancel(self, msg: dict[str, Any]) -> None:
        """Signal cancellation to inflight call."""
        hook_id: int = msg.get("id", 0)
        if hook_id in self.inflight:
            print(f"Cancelling call {hook_id}", file=sys.stderr)
            self.inflight[hook_id].set()

    def _send_error(self, hook_id: int, message: str) -> None:
        """Send error response."""
        self._write_message({
            "t": "done",
            "id": hook_id,
            "content": [{"type": "text", "text": message}],
            "is_error": True,
        })

    def _send_cancelled(self, hook_id: int) -> None:
        """Send cancelled response."""
        self._write_message({
            "t": "done",
            "id": hook_id,
            "content": [{"type": "text", "text": "cancelled"}],
            "is_error": True,
        })

    def _shutdown(self) -> None:
        """Clean shutdown: cancel inflight calls and stop MCP servers."""
        # Stop config watcher
        self._watcher_stop.set()
        if self._watcher_thread:
            self._watcher_thread.join(timeout=1.0)
        
        # Cancel all inflight calls
        for event in self.inflight.values():
            event.set()

        # Stop MCP servers
        for name, client in self.mcp_clients.items():
            try:
                client.shutdown()
            except Exception as e:
                print(f"Error shutting down '{name}': {e}", file=sys.stderr)

    # ── Hot reload via declare (R-PLUG2-110) ───────────────────────────────────

    def _start_config_watcher(self) -> None:
        """Start background thread to watch mcp.toml for changes."""
        self._watcher_thread = threading.Thread(
            target=self._config_watcher_loop,
            daemon=True,
            name="mcp-config-watcher",
        )
        self._watcher_thread.start()
        print("Config watcher started", file=sys.stderr)

    def _config_watcher_loop(self) -> None:
        """Poll mcp.toml mtime and reload on change."""
        while not self._watcher_stop.wait(CONFIG_WATCH_INTERVAL_SECS):
            try:
                new_mtime = mcp_config_mtime()
                if new_mtime is not None and new_mtime != self._config_mtime:
                    print(f"Config file changed (mtime: {self._config_mtime} -> {new_mtime})", file=sys.stderr)
                    self._config_mtime = new_mtime
                    self._reload_config()
            except Exception as e:
                print(f"Config watcher error: {e}", file=sys.stderr)

    def _disconnect(self, name: str) -> None:
        """Shut down a server and drop its tools."""
        client = self.mcp_clients.pop(name, None)
        if client is not None:
            try:
                client.shutdown()
            except Exception as e:
                print(f"Error shutting down '{name}': {e}", file=sys.stderr)
        self.mcp_configs.pop(name, None)
        self.tools = {k: v for k, v in self.tools.items() if v.mcp_server != name}

    def _connect_and_discover(self, cfg: McpServerConfig) -> None:
        """Connect to one server and register its tools. Failures are logged, not raised."""
        try:
            client = self._make_client(cfg)
            client.discover()
            self.mcp_clients[cfg.name] = client
            self.mcp_configs[cfg.name] = cfg
            print(f"Connected to MCP server '{cfg.name}' ({cfg.type})", file=sys.stderr)

            # Discover tools from this server
            try:
                mcp_tools = client.list_tools()
                for tool in mcp_tools:
                    prefixed_name = f"mcp_{cfg.name}_{tool.name}"
                    self.tools[prefixed_name] = ToolSpec(
                        name=prefixed_name,
                        description=f"[{cfg.name}] {tool.description}",
                        schema=tool.input_schema,
                        mcp_server=cfg.name,
                        mcp_tool_name=tool.name,
                    )
                print(f"Discovered {len(mcp_tools)} tool(s) from '{cfg.name}'", file=sys.stderr)
            except McpError as e:
                print(f"Failed to list tools from '{cfg.name}': {e}", file=sys.stderr)

        except McpError as e:
            print(f"Failed to connect to MCP server '{cfg.name}': {e}", file=sys.stderr)
        except Exception as e:
            print(f"Unexpected error connecting to '{cfg.name}': {e}", file=sys.stderr)

    def _reload_config(self) -> None:
        """Reload MCP servers from config and send declare message."""
        print("Reloading MCP configuration...", file=sys.stderr)
        
        old_tool_names = set(self.tools.keys())
        
        # Load new config
        configs = load_mcp_config()
        new_by_name = {c.name: c for c in configs}
        
        # Shut down servers removed from the config.
        for name in list(self.mcp_clients.keys()):
            if name not in new_by_name:
                print(f"Removing MCP server '{name}'", file=sys.stderr)
                self._disconnect(name)
        
        # Connect new servers, and reconnect any whose definition changed — a
        # changed URL, header, cmd or env is invisible to a live connection, so
        # an edited server must be torn down and rebuilt, not skipped.
        for cfg in configs:
            if cfg.name in self.mcp_clients:
                if self.mcp_configs.get(cfg.name) == cfg:
                    continue  # Connected, unchanged
                print(f"MCP server '{cfg.name}' config changed, reconnecting", file=sys.stderr)
                self._disconnect(cfg.name)
            else:
                print(f"Adding MCP server '{cfg.name}'", file=sys.stderr)
            self._connect_and_discover(cfg)
        
        new_tool_names = set(self.tools.keys())
        tools_added = list(new_tool_names - old_tool_names)
        tools_removed = list(old_tool_names - new_tool_names)
        
        if tools_added or tools_removed:
            print(f"Tools changed: +{len(tools_added)} -{len(tools_removed)}", file=sys.stderr)
            self._send_declare(tools_added, tools_removed)
        else:
            print("No tool changes detected", file=sys.stderr)

    def _send_declare(self, tools_added: list[str], tools_removed: list[str]) -> None:
        """Send declare message to hot-update tools with the host."""
        # Build full tool list (same format as hello)
        server_info = ", ".join(
            f"{name} ({len([t for t in self.tools.values() if t.mcp_server == name])} tools)"
            for name in self.mcp_clients.keys()
        )
        
        # Meta-tools
        list_servers = dict(LIST_SERVERS_SPEC)
        list_servers["description"] = (
            f"List available MCP servers: {server_info}. "
            "Use mcp_search_tools to discover tools from a specific server."
        )
        list_servers["hidden"] = False
        
        search_tools = dict(SEARCH_TOOLS_SPEC)
        search_tools["hidden"] = False
        
        # MCP tools (hidden)
        mcp_tools = [
            {
                "name": t.name,
                "description": t.description,
                "schema": t.schema,
                "parallel_safe": True,
                "hidden": True,
            }
            for t in self.tools.values()
        ]
        
        all_tools = [list_servers, search_tools] + mcp_tools
        
        self._write_message({
            "t": "declare",
            "tools": all_tools,
        })
        
        print(f"Sent declare: {len(all_tools)} tools (+{len(tools_added)} -{len(tools_removed)})", file=sys.stderr)
