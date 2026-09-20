"""Configuration loader for MCP servers.

Reads ~/.kn9t/mcp.toml to discover which MCP servers to spawn.

Supports two server types:
- local: subprocess servers (spawn with cmd)
- remote: HTTP/SSE servers (connect to url)

Example config:

    # Local subprocess server
    [[mcp]]
    name = "my-server"
    type = "local"  # default if omitted
    cmd = ["python", "-m", "my_mcp.server"]
    [mcp.env]
    SERVER_URL = "https://example.com"

    # Remote HTTP server
    [[mcp]]
    name = "remote-server"
    type = "remote"
    url = "https://mcp.example.com/mcp"
    [mcp.headers]
    api-key = "env:MCP_API_KEY"
    User-Agent = "optional override of the default kn9t-mcp UA"

Environment variable resolution:
    - "env:VAR_NAME" → reads from os.environ["VAR_NAME"]
    - Any other string → used verbatim
"""

from __future__ import annotations

import os
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal


@dataclass
class McpServerConfig:
    """Configuration for a single MCP server."""

    name: str
    type: Literal["local", "remote"] = "local"
    
    # For local servers (subprocess)
    cmd: list[str] = field(default_factory=list)
    env: dict[str, str] = field(default_factory=dict)
    
    # For remote servers (HTTP/SSE)
    url: str = ""
    headers: dict[str, str] = field(default_factory=dict)
    
    # Common
    timeout_ms: int = 30000
    enabled: bool = True


def resolve_env_value(value: Any) -> str:
    """Resolve environment variable references in config values.
    
    Args:
        value: Config value, may be "env:VAR_NAME" to read from environment.
        
    Returns:
        Resolved string value.
    """
    if isinstance(value, str) and value.startswith("env:"):
        env_var = value[4:]
        resolved = os.environ.get(env_var)
        if resolved is None:
            print(f"Warning: environment variable {env_var} not set", file=sys.stderr)
            return ""
        return resolved
    return str(value)


def mcp_config_path() -> Path:
    """Return the path to the MCP config file."""
    return Path.home() / ".kn9t" / "mcp.toml"


def mcp_config_mtime() -> float | None:
    """Return the mtime of the config file, or None if it doesn't exist."""
    path = mcp_config_path()
    if path.exists():
        return path.stat().st_mtime
    return None


def load_mcp_config() -> list[McpServerConfig]:
    """Load MCP server configurations from ~/.kn9t/mcp.toml.
    
    Returns:
        List of MCP server configurations. Empty list if config file doesn't exist.
    """
    config_path = mcp_config_path()

    if not config_path.exists():
        print(f"No MCP config found at {config_path}", file=sys.stderr)
        return []

    try:
        with open(config_path, "rb") as f:
            config = tomllib.load(f)
    except tomllib.TOMLDecodeError as e:
        print(f"Error parsing {config_path}: {e}", file=sys.stderr)
        return []

    servers: list[McpServerConfig] = []
    
    for server in config.get("mcp", []):
        # Required fields
        name = server.get("name")
        if not name:
            print("Warning: MCP server entry missing 'name', skipping", file=sys.stderr)
            continue

        # Check if enabled (default True)
        enabled = server.get("enabled", True)
        if not enabled:
            print(f"MCP server '{name}' is disabled, skipping", file=sys.stderr)
            continue

        # Determine type
        server_type = server.get("type", "local")
        
        if server_type == "local":
            cmd = server.get("cmd")
            if not cmd:
                print(f"Warning: local MCP server '{name}' missing 'cmd', skipping", file=sys.stderr)
                continue

            # Resolve environment variables
            env: dict[str, str] = {}
            for k, v in server.get("env", {}).items():
                env[k] = resolve_env_value(v)

            servers.append(McpServerConfig(
                name=name,
                type="local",
                cmd=cmd,
                env=env,
                timeout_ms=server.get("timeout_ms", 30000),
            ))
            
        elif server_type == "remote":
            url = server.get("url")
            if not url:
                print(f"Warning: remote MCP server '{name}' missing 'url', skipping", file=sys.stderr)
                continue

            # Resolve headers (may contain env vars)
            headers: dict[str, str] = {}
            for k, v in server.get("headers", {}).items():
                headers[k] = resolve_env_value(v)

            servers.append(McpServerConfig(
                name=name,
                type="remote",
                url=url,
                headers=headers,
                timeout_ms=server.get("timeout_ms", 30000),
            ))
            
        else:
            print(f"Warning: MCP server '{name}' has unknown type '{server_type}', skipping", file=sys.stderr)

    print(f"Loaded {len(servers)} MCP server(s) from config", file=sys.stderr)
    return servers
