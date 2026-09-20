"""Tests for skill parsing and validation."""

from pathlib import Path
import tempfile
import pytest

from kn9t_skills.skill import (
    parse_skill,
    discover_skills,
    SkillParseError,
    NAME_PATTERN,
)


class TestNameValidation:
    """Test skill name validation."""
    
    def test_valid_names(self):
        """Valid names should match the pattern."""
        valid = [
            "pdf-processing",
            "data-analysis",
            "code-review",
            "a",
            "a1",
            "test123",
            "my-skill-v2",
        ]
        for name in valid:
            assert NAME_PATTERN.match(name), f"{name} should be valid"
    
    def test_invalid_names(self):
        """Invalid names should not match the pattern."""
        invalid = [
            "PDF-Processing",  # uppercase
            "-pdf",            # starts with hyphen
            "pdf-",            # ends with hyphen
            "pdf--processing", # consecutive hyphens
            "pdf_processing",  # underscore
            "pdf.processing",  # dot
            "pdf processing",  # space
            "",                # empty
        ]
        for name in invalid:
            assert not NAME_PATTERN.match(name), f"{name} should be invalid"
    
    def test_consecutive_hyphens_rejected(self):
        """Consecutive hyphens should be rejected."""
        assert not NAME_PATTERN.match("pdf--processing")
        assert not NAME_PATTERN.match("a--b--c")
        assert NAME_PATTERN.match("pdf-processing")  # single hyphen OK
        assert NAME_PATTERN.match("a-b-c")  # multiple single hyphens OK


class TestParseSkill:
    """Test SKILL.md parsing."""
    
    def test_minimal_skill(self, tmp_path: Path):
        """Parse a minimal valid skill."""
        skill_dir = tmp_path / "test-skill"
        skill_dir.mkdir()
        skill_file = skill_dir / "SKILL.md"
        skill_file.write_text("""---
name: test-skill
description: A test skill for testing.
---

## Instructions

Do the thing.
""")
        skill = parse_skill(skill_file)
        
        assert skill.name == "test-skill"
        assert skill.description == "A test skill for testing."
        assert "Do the thing" in skill.body
    
    def test_full_skill(self, tmp_path: Path):
        """Parse a skill with all optional fields."""
        skill_dir = tmp_path / "full-skill"
        skill_dir.mkdir()
        skill_file = skill_dir / "SKILL.md"
        skill_file.write_text("""---
name: full-skill
description: A complete skill with all fields.
license: MIT
compatibility: Requires Python 3.11+
metadata:
  author: test-org
  version: "1.0"
allowed-tools: Bash(python:*) Read
---

## Full Instructions

Complete instructions here.
""")
        skill = parse_skill(skill_file)
        
        assert skill.name == "full-skill"
        assert skill.metadata.license == "MIT"
        assert skill.metadata.compatibility == "Requires Python 3.11+"
        assert skill.metadata.metadata["author"] == "test-org"
        assert skill.metadata.metadata["version"] == "1.0"
        assert skill.metadata.allowed_tools == "Bash(python:*) Read"
    
    def test_missing_frontmatter(self, tmp_path: Path):
        """Reject file without frontmatter."""
        skill_dir = tmp_path / "no-frontmatter"
        skill_dir.mkdir()
        skill_file = skill_dir / "SKILL.md"
        skill_file.write_text("# Just Markdown\n\nNo frontmatter here.")
        
        with pytest.raises(SkillParseError, match="Missing YAML frontmatter"):
            parse_skill(skill_file)
    
    def test_missing_name(self, tmp_path: Path):
        """Reject skill without name field."""
        skill_dir = tmp_path / "no-name"
        skill_dir.mkdir()
        skill_file = skill_dir / "SKILL.md"
        skill_file.write_text("""---
description: Missing the name field.
---

Body content.
""")
        with pytest.raises(SkillParseError, match="Missing required field 'name'"):
            parse_skill(skill_file)
    
    def test_missing_description(self, tmp_path: Path):
        """Reject skill without description field."""
        skill_dir = tmp_path / "no-desc"
        skill_dir.mkdir()
        skill_file = skill_dir / "SKILL.md"
        skill_file.write_text("""---
name: no-desc
---

Body content.
""")
        with pytest.raises(SkillParseError, match="Missing required field 'description'"):
            parse_skill(skill_file)
    
    def test_name_directory_mismatch(self, tmp_path: Path):
        """Reject skill where name doesn't match directory."""
        skill_dir = tmp_path / "wrong-dir"
        skill_dir.mkdir()
        skill_file = skill_dir / "SKILL.md"
        skill_file.write_text("""---
name: different-name
description: Name doesn't match directory.
---

Body.
""")
        with pytest.raises(SkillParseError, match="doesn't match directory"):
            parse_skill(skill_file)
    
    def test_invalid_name_format(self, tmp_path: Path):
        """Reject skill with invalid name format."""
        skill_dir = tmp_path / "BadName"
        skill_dir.mkdir()
        skill_file = skill_dir / "SKILL.md"
        skill_file.write_text("""---
name: BadName
description: Uppercase name.
---

Body.
""")
        with pytest.raises(SkillParseError, match="is invalid"):
            parse_skill(skill_file)
    
    def test_name_too_long(self, tmp_path: Path):
        """Reject skill with name exceeding max length."""
        long_name = "a" * 65
        skill_dir = tmp_path / long_name
        skill_dir.mkdir()
        skill_file = skill_dir / "SKILL.md"
        skill_file.write_text(f"""---
name: {long_name}
description: Name too long.
---

Body.
""")
        with pytest.raises(SkillParseError, match="exceeds 64 characters"):
            parse_skill(skill_file)


class TestDiscoverSkills:
    """Test skill discovery."""
    
    def test_discover_multiple(self, tmp_path: Path):
        """Discover multiple skills from a directory."""
        # Create skill 1
        s1 = tmp_path / "skill-one"
        s1.mkdir()
        (s1 / "SKILL.md").write_text("""---
name: skill-one
description: First skill.
---
Body 1.
""")
        
        # Create skill 2
        s2 = tmp_path / "skill-two"
        s2.mkdir()
        (s2 / "SKILL.md").write_text("""---
name: skill-two
description: Second skill.
---
Body 2.
""")
        
        skills = discover_skills([tmp_path])
        
        assert len(skills) == 2
        assert "skill-one" in skills
        assert "skill-two" in skills
    
    def test_skip_invalid(self, tmp_path: Path):
        """Skip invalid skills but continue discovery."""
        # Valid skill
        s1 = tmp_path / "valid-skill"
        s1.mkdir()
        (s1 / "SKILL.md").write_text("""---
name: valid-skill
description: A valid skill.
---
Body.
""")
        
        # Invalid skill (missing description)
        s2 = tmp_path / "invalid-skill"
        s2.mkdir()
        (s2 / "SKILL.md").write_text("""---
name: invalid-skill
---
Body.
""")
        
        skills = discover_skills([tmp_path])
        
        assert len(skills) == 1
        assert "valid-skill" in skills
    
    def test_skip_nonexistent_path(self, tmp_path: Path):
        """Skip paths that don't exist."""
        nonexistent = tmp_path / "does-not-exist"
        
        skills = discover_skills([nonexistent])
        
        assert len(skills) == 0


class TestSkillReferences:
    """Test skill reference file handling."""
    
    def test_list_references(self, tmp_path: Path):
        """List reference files in skill directories."""
        skill_dir = tmp_path / "ref-skill"
        skill_dir.mkdir()
        
        # Create SKILL.md
        (skill_dir / "SKILL.md").write_text("""---
name: ref-skill
description: Skill with references.
---
Body.
""")
        
        # Create reference files
        (skill_dir / "scripts").mkdir()
        (skill_dir / "scripts" / "extract.py").write_text("# Python script")
        
        (skill_dir / "references").mkdir()
        (skill_dir / "references" / "API.md").write_text("# API docs")
        
        skill = parse_skill(skill_dir / "SKILL.md")
        refs = skill.list_references()
        
        # Normalize to forward slashes for cross-platform comparison
        refs_normalized = [r.replace("\\", "/") for r in refs]
        assert "scripts/extract.py" in refs_normalized
        assert "references/API.md" in refs_normalized
    
    def test_get_reference_path(self, tmp_path: Path):
        """Get valid reference paths."""
        skill_dir = tmp_path / "path-skill"
        skill_dir.mkdir()
        
        (skill_dir / "SKILL.md").write_text("""---
name: path-skill
description: Test paths.
---
Body.
""")
        
        (skill_dir / "scripts").mkdir()
        script = skill_dir / "scripts" / "test.py"
        script.write_text("# test")
        
        skill = parse_skill(skill_dir / "SKILL.md")
        
        # Valid path
        path = skill.get_reference_path("scripts/test.py")
        assert path is not None
        assert path == script.resolve()
        
        # Nonexistent path
        assert skill.get_reference_path("nonexistent.txt") is None
        
        # Path escape attempt
        assert skill.get_reference_path("../other/file.txt") is None
