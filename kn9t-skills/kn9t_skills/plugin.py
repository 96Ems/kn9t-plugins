"""kn9t plugin v2 protocol implementation for Agent Skills.

This plugin implements the Agent Skills specification (https://agentskills.io):
- Discovers SKILL.md files from configured directories
- Automatically injects skill catalog into context (once per session)
- Exposes tools for activating skills and reading reference files

The skill catalog (names + descriptions) is injected via get_steering on the
first turn of each session. This keeps context usage minimal (~100 tokens per
skill) while making all skills visible to the agent without needing a tool call.

Protocol overview (stdin/stdout, newline-delimited JSON):

    Host -> Plugin:
        {"t": "hello", "proto": 1, "kn9t": "0.1.0"}
        {"t": "hook", "id": N, "hook": "tool_call", "payload": {...}}
        {"t": "hook", "id": N, "hook": "get_steering", "payload": {...}}
        {"t": "event", "kind": "Compacted", "session": "..."}
        {"t": "shutdown"}

    Plugin -> Host:
        {"t": "hello", "name": "...", "capabilities": [...], "tools": [...]}
        {"t": "done", "id": N, "content": [...], "is_error": bool}
        {"t": "result", "id": N, "messages": [...]}  # for get_steering hook

Reference: kn9t/spec/08b-plugin-redesign.md
"""

from __future__ import annotations

import json
import sys
import threading
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from kn9t_skills.config import SkillsConfig, load_config
from kn9t_skills.skill import Skill, discover_skills


# ── Tool specifications ───────────────────────────────────────────────────────

SKILLS_ACTIVATE_SPEC = {
    "name": "skills_activate",
    "description": (
        "Activate an Agent Skill by name to load its full instructions. "
        "After activation, the skill's detailed guidance will be injected into context. "
        "Use skills_list first to see available skills."
    ),
    "schema": {
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "The skill name to activate (e.g., 'pdf-processing')",
            },
        },
        "required": ["name"],
    },
    "parallel_safe": True,
}

SKILLS_READ_REF_SPEC = {
    "name": "skills_read_reference",
    "description": (
        "Read a reference file from an activated skill. "
        "Reference files include scripts, documentation, and assets bundled with the skill. "
        "Use skills_activate first, then check the skill's listed references."
    ),
    "schema": {
        "type": "object",
        "properties": {
            "skill": {
                "type": "string",
                "description": "The activated skill name",
            },
            "path": {
                "type": "string",
                "description": "Relative path to the reference file (e.g., 'scripts/extract.py')",
            },
        },
        "required": ["skill", "path"],
    },
    "parallel_safe": True,
}


@dataclass
class SessionState:
    """Per-session state for skill management."""
    catalog_injected: bool = False
    activated_skills: set[str] = field(default_factory=set)
    pending_injections: list[tuple[str, str]] = field(default_factory=list)
    # Skills discovered in *this* session's project directory.
    #
    # Kept per session rather than merged into the global dict: one plugin process serves
    # every session, so merging leaked project A's skills into project B's catalogue and
    # offered them to the agent as if they were local. Discovery is keyed on cwd, so the
    # result belongs to the session, not the process.
    project_skills: dict[str, Skill] = field(default_factory=dict)
    # cwd already scanned, so a second get_steering does not re-walk the tree.
    discovered_cwd: str | None = None


class Plugin:
    """kn9t plugin that implements Agent Skills support.

    This plugin:
    1. Discovers SKILL.md files from configured directories at startup
    2. Injects skill catalog (names + descriptions) on first turn per session
    3. Exposes tools to activate skills and read their references
    4. Injects full skill instructions when activated
    """

    def __init__(self, config: SkillsConfig, skills: dict[str, Skill]) -> None:
        self.config = config
        self.skills = skills
        self._stdin_lock = threading.Lock()
        self._stdout_lock = threading.Lock()

        self._sessions: dict[str, SessionState] = {}
        self._sessions_lock = threading.Lock()

    @classmethod
    def from_config(cls) -> Plugin:
        """Load config and discover skills."""
        config = load_config()

        if not config.paths:
            print("No skill paths configured", file=sys.stderr)
            return cls(config, {})

        skills = discover_skills(config.paths)
        print(f"Discovered {len(skills)} skill(s) at startup", file=sys.stderr)

        return cls(config, skills)

    def _get_session(self, session_id: str) -> SessionState:
        """Get or create session state."""
        with self._sessions_lock:
            if session_id not in self._sessions:
                self._sessions[session_id] = SessionState()
            return self._sessions[session_id]

    def _skills_for(self, session: SessionState) -> dict[str, Skill]:
        """The skills visible to one session: global, plus that session's project-local.

        Project skills shadow global ones of the same name -- a project that ships its own
        version of a skill means it.
        """
        return {**self.skills, **session.project_skills}

    def _discover_project_skills(self, session: SessionState, cwd: str) -> None:
        """Discover skills from the session's project directory.

        `cwd` comes from the host payload. It is never inferred: this plugin is one
        long-lived subprocess shared by every session, so `Path.cwd()` is the server's
        directory and would attribute another project's skills to this session.

        Results land in `session.project_skills`, not the global dict, so two sessions in
        different repos each see their own.
        """
        from pathlib import Path

        from kn9t_skills.config import project_skill_paths
        from kn9t_skills.skill import discover_skills

        if session.discovered_cwd == cwd:
            return

        cwd_path = Path(cwd)
        if not cwd_path.exists():
            print(f"Session cwd does not exist, skipping: {cwd}", file=sys.stderr)
            return

        session.discovered_cwd = cwd
        existing_paths = [p for p in project_skill_paths(cwd_path) if p.exists()]
        if not existing_paths:
            return

        print(f"Discovering project skills from {cwd}", file=sys.stderr)
        for name, skill in discover_skills(existing_paths).items():
            session.project_skills[name] = skill
            print(f"Discovered project skill: {name}", file=sys.stderr)

    def run(self) -> None:
        """Main loop: read stdin, dispatch, write stdout."""
        hello = self._read_message()
        if hello is None or hello.get("t") != "hello":
            print("Invalid handshake from host", file=sys.stderr)
            return

        proto = hello.get("proto", 0)
        kn9t_version = hello.get("kn9t", "unknown")
        print(f"Connected to kn9t {kn9t_version} (proto {proto})", file=sys.stderr)

        self._send_hello()

        while True:
            msg = self._read_message()
            if msg is None:
                print("stdin closed, shutting down", file=sys.stderr)
                break

            t = msg.get("t", "")

            if t == "shutdown":
                print("Received shutdown", file=sys.stderr)
                break
            elif t == "hook":
                self._handle_hook(msg)
            elif t == "event":
                self._handle_event(msg)

    def _read_message(self) -> dict[str, Any] | None:
        """Read one JSON line from stdin (thread-safe)."""
        with self._stdin_lock:
            try:
                line = sys.stdin.readline()
            except Exception as e:
                print(f"Error reading stdin: {e}", file=sys.stderr)
                return None

        if not line:
            return None

        try:
            return json.loads(line)
        except json.JSONDecodeError as e:
            print(f"Invalid JSON from host: {e}", file=sys.stderr)
            return None

    def _write_message(self, msg: dict[str, Any]) -> None:
        """Write one JSON line to stdout (thread-safe)."""
        with self._stdout_lock:
            try:
                sys.stdout.write(json.dumps(msg, separators=(",", ":")) + "\n")
                sys.stdout.flush()
            except Exception as e:
                print(f"Error writing stdout: {e}", file=sys.stderr)

    def _send_hello(self) -> None:
        """Send plugin hello with tools."""
        tools = [SKILLS_ACTIVATE_SPEC, SKILLS_READ_REF_SPEC]

        self._write_message({
            "t": "hello",
            "name": "kn9t-skills",
            "capabilities": [],
            "hooks": ["get_steering"],
            "tools": tools,
            "events": ["Compacted"],
        })

        print(f"Registered {len(tools)} tools, {len(self.skills)} skills available", file=sys.stderr)

    def _handle_hook(self, msg: dict[str, Any]) -> None:
        """Dispatch hook invocation."""
        hook_id: int = msg.get("id", 0)
        hook_name: str = msg.get("hook", "")
        payload: dict[str, Any] = msg.get("payload", {})

        if hook_name == "tool_call":
            tool_name: str = payload.get("tool", "")
            args: dict[str, Any] = payload.get("args", {})
            session_id: str = payload.get("session", "_default")

            if tool_name == "skills_activate":
                self._handle_skills_activate(hook_id, args, session_id)
            elif tool_name == "skills_read_reference":
                self._handle_skills_read_ref(hook_id, args, session_id)
            else:
                self._send_error(hook_id, f"Unknown tool: {tool_name}")

        elif hook_name == "get_steering":
            session_id = payload.get("session_id") or payload.get("session", "_default")
            cwd = payload.get("cwd")
            self._handle_get_steering(hook_id, session_id, cwd)
        else:
            self._write_message({"t": "result", "id": hook_id})

    def _handle_skills_activate(
        self, hook_id: int, args: dict[str, Any], session_id: str
    ) -> None:
        """Handle skills_activate tool call."""
        name = args.get("name", "")

        if not name:
            self._send_error(hook_id, "name parameter is required")
            return

        session = self._get_session(session_id)
        # Global + this session's project skills; a project skill is only activatable by
        # the session whose cwd it was found in.
        skills = self._skills_for(session)

        if name not in skills:
            available = sorted(skills.keys())
            self._send_error(
                hook_id,
                f"Unknown skill: {name}. Available: {', '.join(available[:10])}"
            )
            return

        skill = skills[name]

        if name in session.activated_skills:
            refs = skill.list_references()
            lines = [
                f"Skill '{name}' is already active.",
                "",
                f"Description: {skill.description}",
            ]
            if refs:
                lines.append("")
                lines.append("Available references:")
                for ref in refs[:10]:
                    lines.append(f"  • {ref}")
                if len(refs) > 10:
                    lines.append(f"  ... and {len(refs) - 10} more")

            self._send_success(hook_id, "\n".join(lines))
            return

        session.activated_skills.add(name)
        session.pending_injections.append((name, skill.body))

        refs = skill.list_references()
        lines = [
            f"Activated skill: {name}",
            "",
            f"Description: {skill.description}",
            "",
            "The skill's instructions have been loaded into context.",
        ]

        if skill.metadata.compatibility:
            lines.append("")
            lines.append(f"Compatibility: {skill.metadata.compatibility}")

        if refs:
            lines.append("")
            lines.append("Available references:")
            for ref in refs[:10]:
                lines.append(f"  • {ref}")
            if len(refs) > 10:
                lines.append(f"  ... and {len(refs) - 10} more")
            lines.append("")
            lines.append("Use skills_read_reference(skill='..', path='..') to read them.")

        self._send_success(hook_id, "\n".join(lines))

    def _handle_skills_read_ref(
        self, hook_id: int, args: dict[str, Any], session_id: str
    ) -> None:
        """Handle skills_read_reference tool call."""
        skill_name = args.get("skill", "")
        path = args.get("path", "")

        if not skill_name:
            self._send_error(hook_id, "skill parameter is required")
            return
        if not path:
            self._send_error(hook_id, "path parameter is required")
            return

        session = self._get_session(session_id)
        skills = self._skills_for(session)

        if skill_name not in skills:
            self._send_error(hook_id, f"Unknown skill: {skill_name}")
            return

        skill = skills[skill_name]

        if skill_name not in session.activated_skills:
            self._send_error(
                hook_id,
                f"Skill '{skill_name}' is not activated. Use skills_activate first."
            )
            return

        ref_path = skill.get_reference_path(path)
        if ref_path is None:
            refs = skill.list_references()
            self._send_error(
                hook_id,
                f"Reference not found: {path}\n\n"
                f"Available: {', '.join(refs[:10]) if refs else '(none)'}"
            )
            return

        try:
            content = ref_path.read_text(encoding="utf-8")
            self._send_success(hook_id, f"# {path}\n\n{content}")
        except Exception as e:
            self._send_error(hook_id, f"Failed to read {path}: {e}")

    def _handle_get_steering(self, hook_id: int, session_id: str, cwd: str | None) -> None:
        """Handle get_steering hook.

        Injects:
        1. Skill catalog (once per session, on first call)
        2. Pending skill instructions (when skills are activated)

        Uses `cwd` from the payload to discover project-local skills.
        """
        session = self._get_session(session_id)
        messages = []

        # Discover project-local skills from the host-provided cwd on first call.
        if cwd and not session.catalog_injected:
            self._discover_project_skills(session, cwd)

        skills = self._skills_for(session)

        if not session.catalog_injected and skills:
            catalog = self._build_catalog(skills)
            messages.append({
                "role": "user",
                "silent": True,
                "content": [{"type": "text", "text": catalog}],
            })
            session.catalog_injected = True
            print(f"Injected skill catalog ({len(skills)} skills)", file=sys.stderr)

        for name, body in session.pending_injections:
            skill = skills.get(name)
            if not skill:
                continue

            skill_path = skill.path
            text = self._format_skill_instructions(name, skill.description, body, skill_path)

            messages.append({
                "role": "user",
                "silent": True,
                "content": [{"type": "text", "text": text}],
            })

            print(f"Injected skill instructions: {name} ({len(body)} chars)", file=sys.stderr)

        session.pending_injections.clear()

        self._write_message({
            "t": "result",
            "id": hook_id,
            "messages": messages,
        })

    def _build_catalog(self, skills: dict[str, Skill]) -> str:
        """Build the skill catalog for automatic injection.

        Uses <system-reminder> format consistent with AGENTS.md injection pattern.

        `skills` is the caller's session-scoped view, so a project's skills are advertised
        only to sessions rooted in that project.
        """
        lines = [
            '<system-reminder source="Agent Skills">',
            "# Available Skills",
            "",
            "Use `skills_activate(name='...')` to load a skill's full instructions.",
            "",
        ]

        for skill in sorted(skills.values(), key=lambda s: s.name):
            lines.append(f"- **{skill.name}**: {skill.description}")

        lines.append("</system-reminder>")

        return "\n".join(lines)

    def _format_skill_instructions(
        self, name: str, description: str, body: str, skill_path: Path
    ) -> str:
        """Format skill instructions for injection.

        Uses <system-reminder> with source pointing to the SKILL.md file,
        consistent with how AGENTS.md is injected.
        """
        relative_path = skill_path.name
        try:
            relative_path = str(skill_path)
        except Exception:
            pass

        lines = [
            f'<system-reminder source="SKILL.md: {relative_path} ({name}, {len(body)} lines)">',
            f"# {name}",
            "",
            f"> {description}",
            "",
            body,
            "</system-reminder>",
        ]

        return "\n".join(lines)

    def _handle_event(self, msg: dict[str, Any]) -> None:
        """Handle bus events from host.

        Per spec/08b-plugin-redesign.md §2.5, events are:
        {"t": "event", "kind": "<EventKind>", "<...event fields...>": "..."}
        """
        kind = msg.get("kind", "")
        session_id = msg.get("session", "")

        if kind == "Compacted" and session_id:
            with self._sessions_lock:
                if session_id in self._sessions:
                    del self._sessions[session_id]
            print(f"Cleared session state for {session_id} (compacted)", file=sys.stderr)

    def _send_success(self, hook_id: int, result: str) -> None:
        """Send success response."""
        self._write_message({
            "t": "done",
            "id": hook_id,
            "content": [{"type": "text", "text": result}],
            "is_error": False,
        })

    def _send_error(self, hook_id: int, message: str) -> None:
        """Send error response."""
        self._write_message({
            "t": "done",
            "id": hook_id,
            "content": [{"type": "text", "text": message}],
            "is_error": True,
        })
