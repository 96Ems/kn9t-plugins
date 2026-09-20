# kn9t-skills

Agent Skills plugin for kn9t - implements the [Agent Skills](https://agentskills.io) specification.

Agent Skills are a lightweight, open format for extending AI agent capabilities with 
specialized knowledge and workflows. A skill is a directory containing a `SKILL.md` 
file with metadata and instructions.

## What it does

This plugin enables kn9t to:

1. **Discover skills** from configured directories at startup
2. **Progressive disclosure** - only load name/description initially, full instructions on activation
3. **Activate skills** when tasks match their descriptions
4. **Execute bundled scripts** and reference materials as needed

## Architecture

```
kn9t-server <--plugin v2--> kn9t-skills.py
                 (JSON/stdin/stdout)

Skills directories (all searched by default):
  ~/.kn9t/skills/         # Global skills
  .kn9t/skills/           # Project-local (kn9t convention)
  .agents/skills/         # Project-local (common convention)
  .agents/skill/          # Singular variant
  .skills/                # Hidden directory
  .skill/                 # Singular hidden
  skills/                 # Visible directory
  skill/                  # Singular visible
  + Custom paths from config
```

## Installation

```bash
cd plugins/kn9t-skills
pip install -e .
```

## Configuration

### Step 1: Create skills directory

```bash
mkdir -p ~/.kn9t/skills
```

### Step 2: Add skills

Each skill is a folder with a `SKILL.md` file:

```
~/.kn9t/skills/
├── pdf-processing/
│   └── SKILL.md
├── code-review/
│   └── SKILL.md
└── data-analysis/
    ├── SKILL.md
    ├── scripts/
    │   └── analyze.py
    └── references/
        └── REFERENCE.md
```

### Step 3: Register plugin with kn9t

Add to `~/.kn9t/config.toml`:

```toml
[[plugin]]
name = "kn9t-skills"
cmd = ["python", "-m", "kn9t_skills"]
```

### Step 4: Configure skills paths (optional)

Add to `~/.kn9t/skills.toml`:

```toml
# Directories to search for skills
paths = [
    "~/.kn9t/skills",
    "/shared/team-skills",
]

# Enable auto-activation based on task matching (default: true)
auto_activate = true
```

## Usage

Once configured, skills are automatically discovered. The skill catalog (names + descriptions) is injected into context on the first turn of each session, so the agent always knows what skills are available without needing a tool call.

```
$ kn9t chat

You: I need to extract text from this PDF
[Agent sees skill catalog in context, activates pdf-processing]
[calling skills_activate(name='pdf-processing')...]
[following SKILL.md instructions...]
```

## Creating Skills

Minimal `SKILL.md`:

```markdown
---
name: my-skill
description: What this skill does and when to use it.
---

## Instructions

Step-by-step instructions for the agent to follow...
```

See the [Agent Skills specification](https://agentskills.io/specification) for full details.

## Tools Exposed

| Tool | Description |
|------|-------------|
| `skills_activate` | Activate a skill by name, loading its full instructions |
| `skills_read_reference` | Read a reference file from an activated skill |

The skill catalog is automatically injected via `get_steering` on the first turn — no `skills_list` tool is needed.

## Progressive Disclosure

Skills use progressive disclosure to minimize context usage:

1. **Discovery** (~100 tokens per skill): Only `name` and `description` loaded at startup
2. **Activation** (< 5000 tokens recommended): Full `SKILL.md` body loaded when activated
3. **References** (as needed): Files from `scripts/`, `references/`, `assets/` loaded on demand

## Limitations

- **No remote skills**: Skills must be local directories (no URL fetch)
- **No script execution sandbox**: Scripts run with full permissions
- **Python 3.11+**: Uses `tomllib` from stdlib

## Development

```bash
cd plugins/kn9t-skills
pip install -e ".[dev]"

# Run tests
pytest

# Type check
mypy kn9t_skills

# Lint
ruff check kn9t_skills
```

## License

MIT
