# kn9t-plugins

Plugins for [kn9t](https://github.com/96Ems/kn9t), a minimal coding agent in Rust.

## Plugins

| Plugin | Language | Description |
|--------|----------|-------------|
| `kn9t-anthropic` | Rust | Anthropic Messages API provider |
| `kn9t-agents-md` | Go | Discovers and injects AGENTS.md files into context |
| `kn9t-ask-user` | TypeScript | Interactive user prompts |
| `kn9t-compactor` | TypeScript | Context compaction |
| `kn9t-git-integration` | Rust | Git diff rendering in TUI |
| `kn9t-mcp` | Python | Bridges MCP servers (Jira, GitHub, etc.) |
| `kn9t-plugin-manager` | Rust | Plugin management tools |
| `kn9t-policy` | Python | Safety policy enforcement |
| `kn9t-skills` | Python | Skills/capabilities system |
| `kn9t-subagent` | TypeScript | Nested agent loops |
| `kn9t-websearch` | Rust | Web search integration |

## Installation

Build the plugin you need and copy the binary to `~/.kn9t/plugins/`.

### Rust plugins

```bash
cd kn9t-anthropic
cargo build --release
cp target/release/kn9t-anthropic ~/.kn9t/plugins/
```

### TypeScript plugins

```bash
cd kn9t-compactor
npm install
npm run build
# Add to config.toml with cmd = ["node", "/path/to/dist/main.js"]
```

### Python plugins

```bash
cd kn9t-mcp
pip install -e .
# Add to config.toml with cmd = ["python", "-m", "kn9t_mcp"]
```

### Go plugins

```bash
cd kn9t-agents-md
go build -o kn9t-agents-md .
cp kn9t-agents-md ~/.kn9t/plugins/
```

## Configuration

Add plugins to `~/.kn9t/config.toml`:

```toml
# Rust/Go binary plugins
[[plugin]]
name = "kn9t-anthropic"
# binary in ~/.kn9t/plugins/ is auto-discovered

# Python plugins
[[plugin]]
name = "kn9t-mcp"
cmd  = ["python", "-m", "kn9t_mcp"]

# TypeScript plugins
[[plugin]]
name = "kn9t-compactor"
cmd  = ["node", "/path/to/kn9t-compactor/dist/main.js"]
```

## Writing plugins

See [PLUGIN_DEVELOPMENT.md](https://github.com/96Ems/kn9t/blob/main/docs/PLUGIN_DEVELOPMENT.md) in the main repo.

Protocol: newline-delimited JSON on stdin/stdout.

## License

MIT
