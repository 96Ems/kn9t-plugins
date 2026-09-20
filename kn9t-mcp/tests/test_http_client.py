"""Tests for the HTTP MCP transport.

The plugin must identify itself as kn9t, not as `Python-urllib/<version>`:
several MCP endpoints sit behind Cloudflare bot rules that ban that exact string
with HTTP 403 `browser_signature_banned`, an answer that masquerades as an auth
failure. These tests pin the header so it cannot be silently dropped again.
"""

from __future__ import annotations

import json
from unittest import mock

from kn9t_mcp.mcp_http_client import DEFAULT_USER_AGENT, McpHttpClient


class _FakeResponse:
    """Minimal stand-in for the object `urllib.request.urlopen` returns."""

    def __init__(self, body: bytes, headers: dict[str, str] | None = None) -> None:
        self._body = body
        self.headers = headers or {"Content-Type": "application/json"}

    def read(self) -> bytes:
        return self._body

    def __enter__(self) -> "_FakeResponse":
        return self

    def __exit__(self, *exc: object) -> bool:
        return False


def _header(request, name: str) -> str | None:
    """Case-insensitive header lookup on a urllib Request."""
    for key, value in request.headers.items():
        if key.lower() == name.lower():
            return value
    return None


def _capture_request(client: McpHttpClient, body: str = '{"jsonrpc":"2.0","id":1,"result":{}}'):
    """Run one request through a mocked urlopen and return the Request."""
    captured: dict[str, object] = {}

    def fake_urlopen(request, timeout=None):
        captured["request"] = request
        return _FakeResponse(body.encode("utf-8"))

    with mock.patch(
        "kn9t_mcp.mcp_http_client.urllib.request.urlopen", side_effect=fake_urlopen
    ):
        client._send_request("tools/list")

    return captured["request"]


def test_default_user_agent_identifies_as_kn9t():
    """A request carries a kn9t User-Agent, never urllib's default."""
    client = McpHttpClient(name="remote", url="https://mcp.example.com/mcp")

    request = _capture_request(client)
    agent = _header(request, "User-Agent")

    assert agent == DEFAULT_USER_AGENT
    assert "kn9t" in agent
    assert "Python-urllib" not in agent


def test_config_headers_override_user_agent():
    """A server may override the default User-Agent via [mcp.headers]."""
    client = McpHttpClient(
        name="remote",
        url="https://mcp.example.com/mcp",
        headers={"User-Agent": "custom-agent/1.0"},
    )

    request = _capture_request(client)

    assert _header(request, "User-Agent") == "custom-agent/1.0"


def test_notification_carries_user_agent():
    """Notifications (no response expected) identify as kn9t too."""
    client = McpHttpClient(name="remote", url="https://mcp.example.com/mcp")
    captured: dict[str, object] = {}

    def fake_urlopen(request, timeout=None):
        captured["request"] = request
        return _FakeResponse(b"")

    with mock.patch(
        "kn9t_mcp.mcp_http_client.urllib.request.urlopen", side_effect=fake_urlopen
    ):
        client._send_notification("notifications/initialized", {})

    request = captured["request"]
    assert _header(request, "User-Agent") == DEFAULT_USER_AGENT
    assert json.loads(request.data.decode("utf-8"))["method"] == "notifications/initialized"
