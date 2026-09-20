"""kn9t-mcp: MCP client plugin for kn9t.

This plugin bridges kn9t to the Model Context Protocol (MCP) ecosystem,
allowing kn9t agents to use any MCP-compatible server as a tool source.

Architecture:
    kn9t-server <--kn9t plugin v2--> kn9t-mcp <--MCP stdio--> MCP servers

The plugin:
1. Reads MCP server configs from ~/.kn9t/mcp.toml
2. Spawns each configured MCP server as a subprocess
3. Discovers tools from each server via MCP protocol
4. Exposes all tools to kn9t with prefixed names (mcp_{server}_{tool})
5. Routes tool calls to the appropriate MCP server

This proves kn9t's plugin system is truly language-agnostic: the plugin
is written in pure Python with no Rust dependencies.
"""

__version__ = "0.1.0"
