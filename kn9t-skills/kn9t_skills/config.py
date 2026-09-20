"""Configuration loading for kn9t-skills plugin."""

from __future__ import annotations

import os
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path


@dataclass
class SkillsConfig:
    """Configuration for skills discovery and behavior."""
    
    # Directories to search for skills
    paths: list[Path] = field(default_factory=list)
    
    # Whether to automatically activate skills based on task matching
    auto_activate: bool = True


def get_config_dir() -> Path:
    """Get the kn9t config directory (~/.kn9t)."""
    if home := os.environ.get("HOME"):
        return Path(home) / ".kn9t"
    if home := os.environ.get("USERPROFILE"):
        return Path(home) / ".kn9t"
    # No home directory at all. `Path.cwd()` here is the plugin process's directory,
    # which is not a project root -- but this is the *global* config path, and having
    # somewhere to look beats nothing. Project-local paths never use cwd: see
    # `project_skill_paths`.
    return Path.cwd() / ".kn9t"


def expand_path(path_str: str) -> Path:
    """Expand ~ and environment variables in a path."""
    return Path(os.path.expanduser(os.path.expandvars(path_str)))


# Directory conventions a project may use to hold skills, relative to its root.
# One list, used by both the startup scan and per-session discovery, so the two
# cannot drift apart.
PROJECT_SKILL_CONVENTIONS = (
    (".kn9t", "skills"),
    (".agents", "skills"),
    (".agents", "skill"),
    (".skills",),
    (".skill",),
    ("skills",),
    ("skill",),
)


def project_skill_paths(cwd: Path) -> list[Path]:
    """Project-local skill directories for a session rooted at `cwd`.

    `cwd` is the session's working directory as reported by the host, and it is a required
    argument on purpose. This plugin is a long-lived subprocess serving every session at
    once, so `Path.cwd()` is the directory the *server* was started in: one value, shared by
    all sessions, unrelated to the one being served. Reading it here would discover another
    project's skills and offer them to the agent as this project's.
    """
    return [cwd / Path(*parts) for parts in PROJECT_SKILL_CONVENTIONS]


def get_default_skill_paths() -> list[Path]:
    """Global skill search paths -- those knowable before any session exists.

    Only `~/.kn9t/skills`. Project-local directories are per-session and get added by
    `project_skill_paths(cwd)` once the host says where the session is rooted.
    """
    return [get_config_dir() / "skills"]


def load_config() -> SkillsConfig:
    """Load skills configuration from ~/.kn9t/skills.toml and defaults.
    
    Searches all common skill directory conventions by default.
    """
    config = SkillsConfig()
    
    # Default paths - all conventions
    config.paths = get_default_skill_paths()
    
    # Load config file if it exists
    config_file = get_config_dir() / "skills.toml"
    if config_file.exists():
        try:
            with open(config_file, "rb") as f:
                data = tomllib.load(f)
            
            # Override paths if specified (adds to defaults, doesn't replace)
            if "paths" in data:
                extra_paths = [expand_path(p) for p in data["paths"]]
                # Prepend custom paths so they take priority
                config.paths = extra_paths + [p for p in config.paths if p not in extra_paths]
            
            if "auto_activate" in data:
                config.auto_activate = bool(data["auto_activate"])
            
            print(f"Loaded config from {config_file}", file=sys.stderr)
            
        except Exception as e:
            print(f"Warning: Failed to load {config_file}: {e}", file=sys.stderr)
    
    return config
