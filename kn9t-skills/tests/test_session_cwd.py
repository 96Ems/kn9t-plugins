"""Project skills belong to the session whose cwd they were found in.

This plugin is a single long-lived subprocess serving every session at once, so anything
derived from `Path.cwd()` is the *server's* directory: one value, shared, unrelated to the
session being served. Two failures came out of that:

1. `get_default_skill_paths()` built project-local paths from `Path.cwd()`, so the startup
   scan looked for `.agents/skills` next to the server rather than next to the project.
2. `_discover_project_skills()` merged what it found into the process-wide `self.skills`,
   so project A's skills were advertised to a session working in project B -- and, once
   advertised, were activatable there.

The rule these tests pin: cwd is only ever what the host reported for that session, and a
project skill is visible to that session alone.
"""

from pathlib import Path

from kn9t_skills.config import (
    PROJECT_SKILL_CONVENTIONS,
    get_default_skill_paths,
    project_skill_paths,
)
from kn9t_skills.plugin import Plugin
from kn9t_skills.config import SkillsConfig


def write_skill(root: Path, convention: tuple[str, ...], name: str) -> Path:
    """Create a minimal valid SKILL.md under one of the project conventions."""
    d = root.joinpath(*convention) / name
    d.mkdir(parents=True, exist_ok=True)
    (d / "SKILL.md").write_text(
        f"---\nname: {name}\ndescription: skill {name}\n---\n\nBody of {name}.\n",
        encoding="utf-8",
    )
    return d


def plugin() -> Plugin:
    """A plugin with no global skills, so anything found came from a project."""
    return Plugin(SkillsConfig(), {})


class TestConfigPaths:
    def test_default_paths_are_global_only(self):
        """The startup scan must not invent project paths from the process cwd."""
        paths = get_default_skill_paths()
        assert len(paths) == 1, f"expected only the global path, got {paths}"
        assert paths[0].name == "skills"
        assert paths[0].parent.name == ".kn9t"

    def test_default_paths_never_touch_process_cwd(self):
        """No default path may be relative to where the plugin process happens to run."""
        cwd = Path.cwd().resolve()
        for p in get_default_skill_paths():
            resolved = p.resolve()
            assert not resolved.is_relative_to(cwd) or resolved == cwd, (
                f"{p} is rooted at the process cwd; a session's project is elsewhere"
            )

    def test_project_paths_are_rooted_at_the_given_cwd(self):
        root = Path("/somewhere/project")
        paths = project_skill_paths(root)
        assert len(paths) == len(PROJECT_SKILL_CONVENTIONS)
        for p in paths:
            assert p.is_relative_to(root), f"{p} escaped the session root"

    def test_project_paths_cover_every_convention(self):
        root = Path("/p")
        got = {tuple(p.relative_to(root).parts) for p in project_skill_paths(root)}
        assert got == set(PROJECT_SKILL_CONVENTIONS)


class TestSessionIsolation:
    def test_a_project_skill_is_not_visible_to_another_session(self, tmp_path):
        """The leak: discovery for session A must not reach session B."""
        proj_a = tmp_path / "a"
        proj_b = tmp_path / "b"
        proj_b.mkdir()
        write_skill(proj_a, (".agents", "skills"), "only-in-a")

        p = plugin()
        sess_a = p._get_session("A")
        sess_b = p._get_session("B")

        p._discover_project_skills(sess_a, str(proj_a))
        p._discover_project_skills(sess_b, str(proj_b))

        assert "only-in-a" in p._skills_for(sess_a)
        assert "only-in-a" not in p._skills_for(sess_b), (
            "project A's skill leaked into a session working in project B"
        )
        assert "only-in-a" not in p.skills, (
            "a project skill was merged into the process-wide catalogue"
        )

    def test_each_session_sees_its_own_project_skill(self, tmp_path):
        proj_a = tmp_path / "a"
        proj_b = tmp_path / "b"
        write_skill(proj_a, (".agents", "skills"), "skill-a")
        write_skill(proj_b, (".kn9t", "skills"), "skill-b")

        p = plugin()
        sess_a = p._get_session("A")
        sess_b = p._get_session("B")
        p._discover_project_skills(sess_a, str(proj_a))
        p._discover_project_skills(sess_b, str(proj_b))

        assert set(p._skills_for(sess_a)) == {"skill-a"}
        assert set(p._skills_for(sess_b)) == {"skill-b"}

    def test_global_skills_are_shared_by_every_session(self, tmp_path):
        """Isolation applies to project skills; global ones are still global."""
        from kn9t_skills.skill import discover_skills

        global_root = tmp_path / "global"
        write_skill(global_root, ("skills",), "everywhere")
        globals_ = discover_skills([global_root / "skills"])

        p = Plugin(SkillsConfig(), globals_)
        sess_a = p._get_session("A")
        sess_b = p._get_session("B")

        assert "everywhere" in p._skills_for(sess_a)
        assert "everywhere" in p._skills_for(sess_b)

    def test_a_project_skill_shadows_a_global_one_of_the_same_name(self, tmp_path):
        """A project shipping its own version of a skill means it."""
        from kn9t_skills.skill import discover_skills

        global_root = tmp_path / "global"
        write_skill(global_root, ("skills",), "shared")
        globals_ = discover_skills([global_root / "skills"])

        proj = tmp_path / "proj"
        write_skill(proj, (".agents", "skills"), "shared")

        p = Plugin(SkillsConfig(), globals_)
        sess = p._get_session("S")
        p._discover_project_skills(sess, str(proj))

        visible = p._skills_for(sess)["shared"]
        assert visible.path.is_relative_to(proj), "the global skill won over the project's"

    def test_catalog_only_advertises_the_sessions_own_skills(self, tmp_path):
        proj_a = tmp_path / "a"
        proj_b = tmp_path / "b"
        proj_b.mkdir()
        write_skill(proj_a, (".skills",), "private-to-a")

        p = plugin()
        sess_a = p._get_session("A")
        sess_b = p._get_session("B")
        p._discover_project_skills(sess_a, str(proj_a))
        p._discover_project_skills(sess_b, str(proj_b))

        assert "private-to-a" in p._build_catalog(p._skills_for(sess_a))
        assert "private-to-a" not in p._build_catalog(p._skills_for(sess_b))


class TestDiscoveryRobustness:
    def test_a_missing_cwd_directory_discovers_nothing(self, tmp_path):
        p = plugin()
        sess = p._get_session("S")
        p._discover_project_skills(sess, str(tmp_path / "does-not-exist"))
        assert p._skills_for(sess) == {}

    def test_discovery_is_not_repeated_for_the_same_cwd(self, tmp_path):
        """get_steering can fire repeatedly; re-walking the tree each time is waste."""
        proj = tmp_path / "p"
        write_skill(proj, ("skills",), "one")

        p = plugin()
        sess = p._get_session("S")
        p._discover_project_skills(sess, str(proj))
        assert sess.discovered_cwd == str(proj)

        # A second skill appearing after the first scan is not picked up again --
        # the guard is doing its job rather than the filesystem being re-read.
        write_skill(proj, ("skills",), "two")
        p._discover_project_skills(sess, str(proj))
        assert set(p._skills_for(sess)) == {"one"}

    def test_a_project_with_no_skill_directory_is_silent(self, tmp_path):
        proj = tmp_path / "bare"
        proj.mkdir()
        p = plugin()
        sess = p._get_session("S")
        p._discover_project_skills(sess, str(proj))
        assert p._skills_for(sess) == {}
