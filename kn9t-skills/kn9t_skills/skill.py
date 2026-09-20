"""Skill data model and parsing.

Implements the Agent Skills specification (https://agentskills.io/specification):
- YAML frontmatter with name, description, and optional fields
- Markdown body with instructions
- Progressive disclosure (metadata vs full content)

Spec constraints:
- name: 1-64 chars, lowercase alphanumeric + hyphens, no start/end hyphen, no --
- description: 1-1024 chars
- compatibility: 1-500 chars (optional)
- metadata: map of string -> string (optional)
- allowed-tools: space-separated string (optional, experimental)
"""

from __future__ import annotations

import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import yaml


# Frontmatter field constraints from Agent Skills spec
NAME_MAX_LENGTH = 64
NAME_MIN_LENGTH = 1
DESCRIPTION_MAX_LENGTH = 1024
DESCRIPTION_MIN_LENGTH = 1
COMPATIBILITY_MAX_LENGTH = 500

# Name validation per spec:
# - lowercase letters, numbers, hyphens only
# - must not start or end with hyphen
# - must not contain consecutive hyphens
NAME_PATTERN = re.compile(r"^[a-z0-9]+(-[a-z0-9]+)*$")


@dataclass
class SkillMetadata:
    """Skill metadata from SKILL.md frontmatter.
    
    This is the "discovery" level - loaded at startup for all skills.
    Keeps context usage minimal (~100 tokens per skill).
    """
    name: str
    description: str
    license: str | None = None
    compatibility: str | None = None
    metadata: dict[str, str] = field(default_factory=dict)
    allowed_tools: str | None = None
    
    def to_summary(self) -> str:
        """Format for skills_list output."""
        return f"• {self.name}: {self.description}"


@dataclass
class Skill:
    """Full skill with metadata and body content.
    
    Body is loaded only when the skill is activated.
    """
    path: Path
    metadata: SkillMetadata
    body: str
    
    @property
    def name(self) -> str:
        return self.metadata.name
    
    @property
    def description(self) -> str:
        return self.metadata.description
    
    def get_reference_path(self, ref: str) -> Path | None:
        """Get absolute path to a reference file within the skill directory.
        
        Returns None if the path would escape the skill directory.
        """
        skill_dir = self.path.parent
        ref_path = (skill_dir / ref).resolve()
        
        # Security: ensure path stays within skill directory
        try:
            ref_path.relative_to(skill_dir.resolve())
            return ref_path if ref_path.exists() else None
        except ValueError:
            return None
    
    def list_references(self) -> list[str]:
        """List available reference files in scripts/, references/, assets/."""
        skill_dir = self.path.parent
        refs = []
        
        for subdir in ["scripts", "references", "assets"]:
            subdir_path = skill_dir / subdir
            if subdir_path.exists() and subdir_path.is_dir():
                for f in subdir_path.rglob("*"):
                    if f.is_file():
                        refs.append(str(f.relative_to(skill_dir)))
        
        return sorted(refs)


class SkillParseError(Exception):
    """Error parsing a SKILL.md file."""
    pass


def parse_skill(path: Path) -> Skill:
    """Parse a SKILL.md file into a Skill object.
    
    Raises:
        SkillParseError: If the file is invalid
    """
    try:
        content = path.read_text(encoding="utf-8")
    except Exception as e:
        raise SkillParseError(f"Cannot read {path}: {e}") from e
    
    # Parse frontmatter
    if not content.startswith("---"):
        raise SkillParseError(f"{path}: Missing YAML frontmatter (must start with ---)")
    
    parts = content.split("---", 2)
    if len(parts) < 3:
        raise SkillParseError(f"{path}: Invalid frontmatter format")
    
    frontmatter_raw = parts[1].strip()
    body = parts[2].strip()
    
    try:
        frontmatter = yaml.safe_load(frontmatter_raw)
    except yaml.YAMLError as e:
        raise SkillParseError(f"{path}: Invalid YAML frontmatter: {e}") from e
    
    if not isinstance(frontmatter, dict):
        raise SkillParseError(f"{path}: Frontmatter must be a YAML mapping")
    
    # Validate required fields
    name = frontmatter.get("name")
    description = frontmatter.get("description")
    
    if not name:
        raise SkillParseError(f"{path}: Missing required field 'name'")
    if not description:
        raise SkillParseError(f"{path}: Missing required field 'description'")
    
    # Validate name format per Agent Skills spec
    if len(name) < NAME_MIN_LENGTH:
        raise SkillParseError(f"{path}: name must not be empty")
    if len(name) > NAME_MAX_LENGTH:
        raise SkillParseError(f"{path}: name exceeds {NAME_MAX_LENGTH} characters")
    if name.startswith("-") or name.endswith("-"):
        raise SkillParseError(f"{path}: name '{name}' must not start or end with hyphen")
    if "--" in name:
        raise SkillParseError(f"{path}: name '{name}' must not contain consecutive hyphens")
    if not NAME_PATTERN.match(name):
        raise SkillParseError(
            f"{path}: name '{name}' is invalid. Must be lowercase alphanumeric "
            "with single hyphens (e.g., 'pdf-processing')"
        )
    
    # Validate name matches directory name
    if path.parent.name != name:
        raise SkillParseError(
            f"{path}: name '{name}' doesn't match directory name '{path.parent.name}'"
        )
    
    # Validate description per Agent Skills spec
    if len(description) < DESCRIPTION_MIN_LENGTH:
        raise SkillParseError(f"{path}: description must not be empty")
    if len(description) > DESCRIPTION_MAX_LENGTH:
        raise SkillParseError(f"{path}: description exceeds {DESCRIPTION_MAX_LENGTH} characters")
    
    # Validate optional fields
    compatibility = frontmatter.get("compatibility")
    if compatibility and len(compatibility) > COMPATIBILITY_MAX_LENGTH:
        raise SkillParseError(
            f"{path}: compatibility exceeds {COMPATIBILITY_MAX_LENGTH} characters"
        )
    
    metadata_field = frontmatter.get("metadata", {})
    if not isinstance(metadata_field, dict):
        raise SkillParseError(f"{path}: metadata must be a mapping")
    
    # Ensure all metadata values are strings
    metadata_dict = {}
    for k, v in metadata_field.items():
        metadata_dict[str(k)] = str(v)
    
    metadata = SkillMetadata(
        name=name,
        description=description,
        license=frontmatter.get("license"),
        compatibility=compatibility,
        metadata=metadata_dict,
        allowed_tools=frontmatter.get("allowed-tools"),
    )
    
    return Skill(path=path, metadata=metadata, body=body)


def discover_skills(paths: list[Path]) -> dict[str, Skill]:
    """Discover all skills in the given directories.
    
    Returns a dict mapping skill name to Skill object.
    Logs errors for invalid skills but continues discovery.
    """
    import sys
    
    skills: dict[str, Skill] = {}
    
    for base_path in paths:
        if not base_path.exists():
            continue
        
        # Look for SKILL.md in immediate subdirectories
        for skill_dir in base_path.iterdir():
            if not skill_dir.is_dir():
                continue
            
            skill_file = skill_dir / "SKILL.md"
            if not skill_file.exists():
                continue
            
            try:
                skill = parse_skill(skill_file)
                
                if skill.name in skills:
                    print(
                        f"Warning: Duplicate skill '{skill.name}' in {skill_file}, "
                        f"keeping {skills[skill.name].path}",
                        file=sys.stderr
                    )
                    continue
                
                skills[skill.name] = skill
                print(f"Discovered skill: {skill.name}", file=sys.stderr)
                
            except SkillParseError as e:
                print(f"Warning: {e}", file=sys.stderr)
    
    return skills
