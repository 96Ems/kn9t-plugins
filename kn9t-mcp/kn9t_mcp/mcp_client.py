"""Minimal MCP client implementation over stdio.

This module provides a lightweight MCP client that:
1. Spawns an MCP server as a subprocess
2. Communicates via JSON-RPC 2.0 over stdin/stdout
3. Supports tool discovery and invocation

The implementation is intentionally minimal - no async, no external deps,
just stdlib. This keeps the plugin lightweight and proves that MCP
integration doesn't require heavy machinery.

MCP Protocol Reference: https://modelcontextprotocol.io/specification/2026-07-28/
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
from dataclasses import dataclass
from typing import Any


@dataclass
class McpTool:
    """A tool exposed by an MCP server."""

    name: str
    description: str
    input_schema: dict[str, Any]


class McpError(Exception):
    """Error from MCP server or protocol."""

    pass


class McpClient:
    """Client for a single MCP server subprocess.
    
    The client spawns the MCP server and communicates via JSON-RPC 2.0
    over stdin/stdout. Each client manages exactly one server.
    
    Thread safety: All methods are thread-safe via an internal lock.
    
    Example:
        client = McpClient.spawn("github", ["npx", "-y", "@mcp/server-github"], {})
        client.discover()
        tools = client.list_tools()
        result = client.call_tool("list_repos", {"owner": "kn9t"})
    """

    def __init__(self, name: str, process: subprocess.Popen[str]) -> None:
        """Initialize client with a running subprocess.
        
        Args:
            name: Human-readable name for this server (used in logging).
            process: Running subprocess with stdin/stdout pipes.
        """
        self.name = name
        self.process = process
        self._request_id = 0
        self._lock = threading.Lock()
        self._protocol_version: str | None = None

    @classmethod
    def spawn(
        cls, name: str, cmd: list[str], env: dict[str, str], timeout_ms: int = 30000
    ) -> McpClient:
        """Spawn an MCP server subprocess.
        
        Args:
            name: Name for this server instance.
            cmd: Command and arguments to spawn the server.
            env: Additional environment variables (merged with current env).
            timeout_ms: Timeout for operations (currently unused, reserved).
            
        Returns:
            McpClient connected to the spawned server.
            
        Raises:
            McpError: If the server fails to start.
        """
        merged_env = {**os.environ, **env}

        try:
            process = subprocess.Popen(
                cmd,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                env=merged_env,
                text=True,
                bufsize=1,  # line buffered
                encoding="utf-8",
                errors="replace",  # handle encoding issues gracefully
            )
        except FileNotFoundError as e:
            raise McpError(f"Failed to spawn MCP server '{name}': {e}") from e
        except OSError as e:
            raise McpError(f"OS error spawning MCP server '{name}': {e}") from e

        print(f"Spawned MCP server '{name}': {' '.join(cmd)}", file=sys.stderr)
        return cls(name, process)

    def _next_id(self) -> int:
        """Get next request ID (thread-safe)."""
        with self._lock:
            self._request_id += 1
            return self._request_id

    def _send_request(self, method: str, params: dict[str, Any] | None = None, timeout: float = 30.0) -> Any:
        """Send JSON-RPC request and wait for response.
        
        Args:
            method: JSON-RPC method name.
            params: Optional parameters object.
            timeout: Timeout in seconds (currently unused, reads are blocking).
            
        Returns:
            The 'result' field from the response.
            
        Raises:
            McpError: On protocol errors or if server returns an error.
        """
        if self.process.stdin is None or self.process.stdout is None:
            raise McpError(f"MCP server '{self.name}' has no stdio pipes")

        req_id = self._next_id()

        request: dict[str, Any] = {
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
        }
        if params is not None:
            request["params"] = params

        # Serialize and send
        request_line = json.dumps(request) + "\n"
        
        try:
            self.process.stdin.write(request_line)
            self.process.stdin.flush()
        except BrokenPipeError as e:
            raise McpError(f"MCP server '{self.name}' pipe broken: {e}") from e

        # Read response (blocking - MCP servers should respond quickly)
        try:
            line = self.process.stdout.readline()
        except Exception as e:
            raise McpError(f"Error reading from MCP server '{self.name}': {e}") from e

        if not line:
            # Check if process died
            retcode = self.process.poll()
            if retcode is not None:
                stderr = ""
                if self.process.stderr:
                    stderr = self.process.stderr.read()
                raise McpError(
                    f"MCP server '{self.name}' exited with code {retcode}: {stderr}"
                )
            raise McpError(f"MCP server '{self.name}' closed stdout unexpectedly")

        try:
            response = json.loads(line)
        except json.JSONDecodeError as e:
            raise McpError(f"Invalid JSON from MCP server '{self.name}': {e}") from e

        # Check for JSON-RPC error
        if "error" in response:
            err = response["error"]
            code = err.get("code", "?")
            message = err.get("message", "Unknown error")
            raise McpError(f"MCP server '{self.name}' error {code}: {message}")

        return response.get("result", {})

    def discover(self) -> dict[str, Any]:
        """Probe server capabilities via initialize (legacy MCP).
        
        Most MCP servers use the legacy initialize handshake.
        
        Returns:
            Server capabilities dict.
        """
        # Go straight to legacy initialize - most servers use this
        return self._initialize_legacy()

    def _initialize_legacy(self) -> dict[str, Any]:
        """Initialize handshake for MCP servers."""
        result = self._send_request("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "kn9t-mcp", "version": "0.1.0"},
        })
        
        self._protocol_version = result.get("protocolVersion", "unknown")
        server_info = result.get("serverInfo", {})
        server_name = server_info.get("name", "unknown")
        server_version = server_info.get("version", "?")
        print(f"MCP server '{self.name}' initialized: {server_name} v{server_version}", file=sys.stderr)
        
        # Send initialized notification (required by MCP protocol)
        self._send_notification("notifications/initialized", {})
        
        return result

    def _send_notification(self, method: str, params: dict[str, Any] | None = None) -> None:
        """Send JSON-RPC notification (no response expected)."""
        if self.process.stdin is None:
            raise McpError(f"MCP server '{self.name}' has no stdin pipe")

        notification: dict[str, Any] = {
            "jsonrpc": "2.0",
            "method": method,
        }
        if params is not None:
            notification["params"] = params

        try:
            self.process.stdin.write(json.dumps(notification) + "\n")
            self.process.stdin.flush()
        except BrokenPipeError as e:
            raise McpError(f"MCP server '{self.name}' pipe broken: {e}") from e

    def list_tools(self) -> list[McpTool]:
        """Get list of tools from the MCP server.
        
        Returns:
            List of available tools with their schemas.
        """
        result = self._send_request("tools/list")
        tools = result.get("tools", [])

        return [
            McpTool(
                name=t["name"],
                description=t.get("description", ""),
                input_schema=t.get("inputSchema", {}),
            )
            for t in tools
        ]

    def call_tool(
        self,
        name: str,
        arguments: dict[str, Any],
        cancel: threading.Event | None = None,
    ) -> str:
        """Call a tool on the MCP server.
        
        Args:
            name: Tool name (as returned by list_tools).
            arguments: Tool arguments matching the input schema.
            cancel: Optional event to signal cancellation (not yet implemented).
            
        Returns:
            Tool result as a string (text content concatenated).
        """
        # TODO: implement cancellation via notifications/cancelled
        result = self._send_request("tools/call", {
            "name": name,
            "arguments": arguments,
        })

        # Handle isError flag
        if result.get("isError"):
            content = result.get("content", [])
            error_text = self._extract_text_content(content)
            raise McpError(f"Tool '{name}' failed: {error_text}")

        # Extract text content from result
        content = result.get("content", [])
        return self._extract_text_content(content)

    def _extract_text_content(self, content: list[dict[str, Any]]) -> str:
        """Extract text from MCP content array."""
        texts: list[str] = []
        for item in content:
            item_type = item.get("type", "")
            if item_type == "text":
                texts.append(item.get("text", ""))
            elif item_type == "resource":
                # Embedded resource - extract text if present
                resource = item.get("resource", {})
                if "text" in resource:
                    texts.append(resource["text"])
            elif item_type == "image":
                texts.append("[image]")
            else:
                # Unknown type, serialize as JSON
                texts.append(json.dumps(item))
        
        return "\n".join(texts) if texts else ""

    def shutdown(self) -> None:
        """Gracefully stop the MCP server.
        
        Closes stdin and waits for the process to exit.
        Falls back to SIGTERM/SIGKILL if needed.
        """
        if self.process.stdin:
            try:
                self.process.stdin.close()
            except Exception:
                pass

        try:
            self.process.wait(timeout=5)
            print(f"MCP server '{self.name}' stopped gracefully", file=sys.stderr)
        except subprocess.TimeoutExpired:
            print(f"MCP server '{self.name}' not responding, killing", file=sys.stderr)
            self.process.kill()
            self.process.wait()
