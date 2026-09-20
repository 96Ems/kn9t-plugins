"""MCP client implementation over HTTP/SSE for remote servers.

This module provides an HTTP-based MCP client that:
1. Connects to remote MCP servers via HTTP POST + SSE
2. Supports the Streamable HTTP transport (MCP 2025+)
3. Works with servers like testkub that expose /mcp endpoints

MCP Streamable HTTP Transport:
- POST requests to the MCP endpoint with JSON-RPC body
- Server responds with SSE stream or direct JSON response
- Headers can include custom auth (tk-api-key, etc.)

Reference: https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http
"""

from __future__ import annotations

import json
import sys
import threading
import urllib.request
import urllib.error
from dataclasses import dataclass
from typing import Any

from kn9t_mcp import __version__
from kn9t_mcp.mcp_client import McpTool, McpError

# urllib defaults to `User-Agent: Python-urllib/<version>`. Some MCP endpoints sit
# behind Cloudflare bot rules that ban that exact string with HTTP 403
# `Access denied / browser_signature_banned` — which reads like a missing or bad
# token and sends you hunting for an auth bug that isn't there. Identify as kn9t
# instead. A server can still override it via `[mcp.headers]` in mcp.toml; do not
# "clean this up" as redundant, it is load-bearing.
DEFAULT_USER_AGENT = f"kn9t-mcp/{__version__} (+https://github.com/kn9t/kn9t)"


class McpHttpClient:
    """Client for a remote MCP server over HTTP/SSE.
    
    Connects to MCP servers exposed via HTTP endpoints.
    Supports custom headers for authentication.
    
    Example:
        client = McpHttpClient(
            name="my-server",
            url="https://mcp.example.com/mcp",
            headers={"api-key": "env:MCP_API_KEY"}
        )
        client.discover()
        tools = client.list_tools()
    """

    def __init__(
        self,
        name: str,
        url: str,
        headers: dict[str, str] | None = None,
        timeout: float = 30.0,
    ) -> None:
        """Initialize HTTP client.
        
        Args:
            name: Human-readable name for this server.
            url: Base URL for the MCP endpoint (e.g., https://server.com/mcp).
            headers: Custom HTTP headers (auth tokens, etc.).
            timeout: Request timeout in seconds.
        """
        self.name = name
        self.url = url.rstrip("/")
        self.headers = headers or {}
        self.timeout = timeout
        self._request_id = 0
        self._lock = threading.Lock()
        self._protocol_version: str | None = None
        self._session_id: str | None = None

    def _next_id(self) -> int:
        """Get next request ID (thread-safe)."""
        with self._lock:
            self._request_id += 1
            return self._request_id

    def _send_request(self, method: str, params: dict[str, Any] | None = None) -> Any:
        """Send JSON-RPC request over HTTP and wait for response.
        
        Args:
            method: JSON-RPC method name.
            params: Optional parameters object.
            
        Returns:
            The 'result' field from the response.
            
        Raises:
            McpError: On HTTP or protocol errors.
        """
        req_id = self._next_id()

        request_body: dict[str, Any] = {
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
        }
        if params is not None:
            request_body["params"] = params

        # Build HTTP request
        request_data = json.dumps(request_body).encode("utf-8")
        
        http_headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "User-Agent": DEFAULT_USER_AGENT,
            **self.headers,
        }
        
        # Add session ID if we have one
        if self._session_id:
            http_headers["Mcp-Session-Id"] = self._session_id

        req = urllib.request.Request(
            self.url,
            data=request_data,
            headers=http_headers,
            method="POST",
        )

        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as response:
                content_type = response.headers.get("Content-Type", "")
                
                # Check for session ID in response
                session_id = response.headers.get("Mcp-Session-Id")
                if session_id:
                    self._session_id = session_id
                
                # Handle SSE response
                if "text/event-stream" in content_type:
                    return self._parse_sse_response(response, req_id)
                
                # Handle direct JSON response
                response_data = response.read().decode("utf-8")
                return self._parse_json_response(response_data, req_id)
                
        except urllib.error.HTTPError as e:
            body = ""
            try:
                body = e.read().decode("utf-8")
            except Exception:
                pass
            raise McpError(f"MCP server '{self.name}' HTTP {e.code}: {body}") from e
        except urllib.error.URLError as e:
            raise McpError(f"MCP server '{self.name}' connection failed: {e.reason}") from e
        except Exception as e:
            raise McpError(f"MCP server '{self.name}' request failed: {e}") from e

    def _parse_json_response(self, data: str, expected_id: int) -> Any:
        """Parse a direct JSON-RPC response."""
        try:
            response = json.loads(data)
        except json.JSONDecodeError as e:
            raise McpError(f"Invalid JSON from MCP server '{self.name}': {e}") from e

        # Check for JSON-RPC error
        if "error" in response:
            err = response["error"]
            code = err.get("code", "?")
            message = err.get("message", "Unknown error")
            raise McpError(f"MCP server '{self.name}' error {code}: {message}")

        return response.get("result", {})

    def _parse_sse_response(self, response: Any, expected_id: int) -> Any:
        """Parse an SSE stream response, extracting the final result."""
        result = None
        
        for line in response:
            if isinstance(line, bytes):
                line = line.decode("utf-8")
            line = line.strip()
            
            if not line:
                continue
                
            # SSE format: "data: {json}"
            if line.startswith("data:"):
                data = line[5:].strip()
                if not data:
                    continue
                    
                try:
                    event = json.loads(data)
                except json.JSONDecodeError:
                    continue
                
                # Check if this is a JSON-RPC response
                if "jsonrpc" in event:
                    if "error" in event:
                        err = event["error"]
                        code = err.get("code", "?")
                        message = err.get("message", "Unknown error")
                        raise McpError(f"MCP server '{self.name}' error {code}: {message}")
                    
                    if "result" in event:
                        result = event["result"]
        
        if result is None:
            raise McpError(f"MCP server '{self.name}' returned no result in SSE stream")
        
        return result

    def discover(self) -> dict[str, Any]:
        """Initialize connection with MCP server.
        
        Returns:
            Server capabilities dict.
        """
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
        
        # Send initialized notification
        self._send_notification("notifications/initialized", {})
        
        return result

    def _send_notification(self, method: str, params: dict[str, Any] | None = None) -> None:
        """Send JSON-RPC notification over HTTP (no response expected)."""
        notification: dict[str, Any] = {
            "jsonrpc": "2.0",
            "method": method,
        }
        if params is not None:
            notification["params"] = params

        request_data = json.dumps(notification).encode("utf-8")
        
        http_headers = {
            "Content-Type": "application/json",
            "User-Agent": DEFAULT_USER_AGENT,
            **self.headers,
        }
        
        if self._session_id:
            http_headers["Mcp-Session-Id"] = self._session_id

        req = urllib.request.Request(
            self.url,
            data=request_data,
            headers=http_headers,
            method="POST",
        )

        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as response:
                # Notifications don't expect a meaningful response
                pass
        except Exception as e:
            # Log but don't fail on notification errors
            print(f"Warning: notification to '{self.name}' failed: {e}", file=sys.stderr)

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
                resource = item.get("resource", {})
                if "text" in resource:
                    texts.append(resource["text"])
            elif item_type == "image":
                texts.append("[image]")
            else:
                texts.append(json.dumps(item))
        
        return "\n".join(texts) if texts else ""

    def shutdown(self) -> None:
        """Clean up HTTP client (no-op for HTTP clients)."""
        print(f"MCP server '{self.name}' disconnected", file=sys.stderr)
