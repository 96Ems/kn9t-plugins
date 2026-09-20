# kn9t-mcp

MCP (Model Context Protocol) client plugin for kn9t.

This plugin allows kn9t to use tools from any MCP-compatible server, expanding
the agent's capabilities to include integrations with Jira, GitHub, Slack,
databases, filesystems, and hundreds of other services.

## What it proves

This plugin demonstrates that **kn9t's plugin system is truly language-agnostic**.
While kn9t itself is written in Rust, this plugin is pure Python with zero Rust
dependencies. It communicates with kn9t-server using the same stdio JSON protocol
that Rust plugins use.

## Architecture

```
kn9t-server <--plugin v2--> kn9t-mcp.py <--MCP stdio--> MCP servers
                (JSON/stdin/stdout)           (JSON-RPC)
```

1. kn9t-server spawns `kn9t-mcp` as a subprocess
2. `kn9t-mcp` reads config and spawns MCP servers (Jira, GitHub, etc.)
3. `kn9t-mcp` discovers tools from each MCP server
4. Tools are exposed to kn9t with prefixed names (`mcp_github_list_prs`)
5. When the model calls a tool, `kn9t-mcp` routes it to the right MCP server

## Installation

```bash
# From source
cd plugins/kn9t-mcp
pip install -e .

# Or directly
pip install kn9t-mcp  # (future: when published to PyPI)
```

## Configuration

### Step 1: Configure MCP servers

Create `~/.kn9t/mcp.toml`:

```toml
# GitHub integration
[[mcp]]
name = "github"
cmd = ["npx", "-y", "@modelcontextprotocol/server-github"]
[mcp.env]
GITHUB_PERSONAL_ACCESS_TOKEN = "env:GITHUB_TOKEN"

# Jira integration
[[mcp]]
name = "jira"
cmd = ["npx", "-y", "@anthropic/mcp-server-jira"]
[mcp.env]
JIRA_URL = "https://mycompany.atlassian.net"
JIRA_USERNAME = "user@example.com"
JIRA_API_TOKEN = "env:JIRA_TOKEN"

# Local filesystem access
[[mcp]]
name = "fs"
cmd = ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/home/user/projects"]

# SQLite database
[[mcp]]
name = "db"
cmd = ["uvx", "mcp-server-sqlite", "--db-path", "/path/to/data.db"]
```

#### Remote (HTTP/SSE) servers

Servers reachable over HTTP are configured with `type = "remote"`:

```toml
[[mcp]]
name = "remote"
type = "remote"
url = "https://mcp.example.com/mcp"
[mcp.headers]
api-key = "env:MCP_API_KEY"
```

> **Do not remove the `User-Agent` header.** This plugin identifies itself as
> `kn9t-mcp/<version> (+https://github.com/kn9t/kn9t)`. Python's `urllib` default,
> `Python-urllib/3.x`, is banned by some MCP endpoints behind Cloudflare bot rules and
> gets answered with `HTTP 403 Access denied / browser_signature_banned` — an error that
> looks exactly like an auth failure, so it sends you chasing a token bug that isn't
> there. The default lives in `kn9t_mcp/mcp_http_client.py` (`DEFAULT_USER_AGENT`) and can
> be overridden per server with a `User-Agent` key under `[mcp.headers]`.

### Step 2: Register plugin with kn9t

Add to `~/.kn9t/config.toml`:

```toml
[[plugin]]
name = "kn9t-mcp"
cmd = ["python", "-m", "kn9t_mcp"]
```

Or if installed globally:

```toml
[[plugin]]
name = "kn9t-mcp"
cmd = ["kn9t-mcp"]
```

### Hot reload

The plugin watches `~/.kn9t/mcp.toml` and reloads it while kn9t is running — no
restart needed. It polls the file every couple of seconds and, when it changes,
sends a `declare` message to kn9t-server, which rebuilds its tool registry and
tells the TUI (`plugin_declared`). What that means in practice:

| Change to `mcp.toml` | Effect |
|---|---|
| Add a server | Spawned/connected; its tools become callable |
| Remove a server | Disconnected; its tools disappear |
| Edit a server (cmd, env, url, headers) | Reconnected with the new definition |
| Rename a server | Treated as remove + add |

Two things are **not** picked up: a server that starts exposing a *new* tool
while its own config is unchanged (the plugin does not poll each server's tool
list), and changes only to an env var the config already referenced (the file
mtime must change).

## Usage

Once configured, MCP tools appear automatically in kn9t:

```
$ kn9t chat

You: List open PRs in the kn9t repo
[calling mcp_github_list_pull_requests...]
Assistant: Here are the open PRs:
1. #42 - Add MCP support (draft)
2. #41 - Fix token accounting
```

## Tool naming

Tools from MCP servers are prefixed to avoid name collisions:

```
Original MCP tool    ->  kn9t tool name
-------------------------------------------
github/list_prs      ->  mcp_github_list_prs
jira/create_issue    ->  mcp_jira_create_issue
fs/read_file         ->  mcp_fs_read_file
```

## Testing the plugin standalone

You can test the plugin without kn9t by sending it raw JSON:

```bash
# Test handshake
echo '{"t":"hello","proto":1,"kn9t":"0.1.0"}' | python -m kn9t_mcp

# Should output hello response with discovered tools
```

## Supported MCP servers

Any MCP-compatible server works. Popular ones:

| Server | Install | Description |
|--------|---------|-------------|
| GitHub | `npx @modelcontextprotocol/server-github` | PRs, issues, repos |
| Filesystem | `npx @modelcontextprotocol/server-filesystem` | File operations |
| SQLite | `uvx mcp-server-sqlite` | Database queries |
| Postgres | `npx @modelcontextprotocol/server-postgres` | PostgreSQL access |
| Slack | `npx @modelcontextprotocol/server-slack` | Send/read messages |
| Google Drive | `npx @anthropic/mcp-server-gdrive` | Drive file access |
| Brave Search | `npx @anthropic/mcp-server-brave-search` | Web search |

See [MCP servers directory](https://github.com/modelcontextprotocol/servers) for more.

## Limitations

- **No OAuth support yet**: Servers requiring OAuth must be configured with static tokens
- **No resources/prompts**: Only MCP tools are exposed (resources and prompts are not)
- **Python 3.11+**: Uses `tomllib` from stdlib (3.11+) and type hints

## Development

```bash
cd plugins/kn9t-mcp
pip install -e ".[dev]"

# Run tests
pytest

# Type check
mypy kn9t_mcp

# Lint
ruff check kn9t_mcp
```

## License

MIT
