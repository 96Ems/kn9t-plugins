#!/usr/bin/env python3
"""kn9t-policy: Interactive tool approval plugin with TUI.

Modes:
- normal: Use policy rules (ALLOW/DENY/ASK patterns)
- yolo: Allow everything (no prompts)
- ask_all: Ask for every tool call

Grants: User-defined patterns that are always allowed.
"""

import json
import sys
import re
import fnmatch
from pathlib import Path
from typing import Optional
from dataclasses import dataclass, field, asdict

# ══════════════════════════════════════════════════════════════════════════════
# State
# ══════════════════════════════════════════════════════════════════════════════

@dataclass
class Decision:
    tool: str
    cmd: str  # For bash, the command; for others, tool name
    result: str  # "allow", "deny", "ask"
    reason: str = ""

@dataclass
class State:
    mode: str = "normal"  # "normal", "yolo", "ask_all"
    grants: list = field(default_factory=list)  # User-added always-allow patterns
    recent: list = field(default_factory=list)  # Last N decisions
    cursor: int = 0  # For grant list navigation

MAX_RECENT = 10
state = State()
session_id: Optional[str] = None

# ══════════════════════════════════════════════════════════════════════════════
# Persistence
# ══════════════════════════════════════════════════════════════════════════════

def grants_path() -> Path:
    return Path.home() / ".kn9t" / "grants.json"

def load_grants():
    p = grants_path()
    if p.exists():
        try:
            data = json.loads(p.read_text())
            state.grants = data.get("grants", [])
            state.mode = data.get("mode", "normal")
        except Exception as e:
            log(f"grants load error: {e}")

def save_grants():
    p = grants_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps({"grants": state.grants, "mode": state.mode}, indent=2))

# ══════════════════════════════════════════════════════════════════════════════
# Policy rules (built-in)
# ══════════════════════════════════════════════════════════════════════════════

ALLOW = [
    # Read-only commands
    "ls *", "dir *", "pwd", "cd *", "tree *",
    "cat *", "head *", "tail *", "less *", "more *",
    "grep *", "rg *", "find *", "fd *", "which *", "where *",
    "echo *", "printf *",
    # Git read-only
    "git status*", "git log*", "git diff*", "git show*", "git branch*",
    "git remote*", "git stash list*",
    # Build tools (safe)
    "cargo check*", "cargo test*", "cargo build*", "cargo clippy*",
    "npm test*", "npm run*", "pnpm test*", "bun test*",
    "python -c *", "python --version*",
]

DENY = [
    "rm -rf /*", "rm -rf /", "rm -rf ~*",
    "sudo *", "su *",
    "shutdown*", "reboot*", "poweroff*",
    "mkfs*", "dd*of=/dev/*",
]

ASK = [
    # Git destructive
    "git checkout*", "git reset*", "git clean*",
    "git rebase*", "git merge*", "git push*",
    "git stash drop*", "git stash pop*", "git stash clear*",
    "git branch -D*", "git branch -d*",
    # File deletion
    "rm *", "rmdir *", "del *", "Remove-Item*",
    # Publishing
    "npm publish*", "cargo publish*",
]

def matches(value: str, patterns: list) -> bool:
    return any(fnmatch.fnmatch(value, p) for p in patterns)

def split_commands(cmd: str) -> list:
    return [p.strip() for p in re.split(r"\s*(?:;|&&|\|\||\|)\s*", cmd) if p.strip()]

def extract_inner(part: str) -> str:
    """Extract inner command from wrappers like cmd.exe /c '...'"""
    m = re.match(r'cmd\.exe\s+/c\s+["\']?([^"\']+)["\']?', part, re.I)
    return m.group(1) if m else part

# ══════════════════════════════════════════════════════════════════════════════
# Policy check
# ══════════════════════════════════════════════════════════════════════════════

def check(tool: str, args: dict, cwd: str) -> dict:
    """Check tool call against policy. Returns {action, reason}."""
    
    # Mode overrides
    if state.mode == "yolo":
        return {"action": "allow", "reason": "YOLO mode"}
    if state.mode == "ask_all":
        return {"action": "ask", "reason": "Ask-all mode"}
    
    # Only check bash commands in detail
    if tool != "bash":
        return {"action": "allow", "reason": "Non-bash tool"}
    
    cmd = args.get("cmd", args.get("command", ""))
    
    for part in split_commands(cmd):
        inner = extract_inner(part)
        
        # Check user grants first (highest priority) - overrides DENY/ASK
        if matches(inner, state.grants):
            continue  # Granted, check next part
        
        # Check built-in DENY (hard block)
        if matches(inner, DENY):
            return {"action": "deny", "reason": f"Blocked: {inner}"}
        
        # Check built-in ASK (dangerous but allowable)
        if matches(inner, ASK):
            return {"action": "ask", "reason": f"Destructive: {inner}"}
        
        # Everything else is allowed by default (yolo mode logic)
        # Unknown commands are safe unless explicitly in DENY/ASK
    
    return {"action": "allow", "reason": "No dangerous patterns"}

# ══════════════════════════════════════════════════════════════════════════════
# UI
# ══════════════════════════════════════════════════════════════════════════════

UI_LUA = r'''
-- Policy plugin UI
local V = { cursor = 0, adding = false, input = "" }

function on_state(s)
    V.mode = s.mode or "normal"
    V.grants = s.grants or {}
    V.recent = s.recent or {}
    -- Don't override cursor/adding/input from state - those are local UI state
end

-- Exact keys first: the host matches on_key before on_text, so a shortcut aimed
-- at a printable character must decline while a grant is being typed. That
-- leaves the single on_text handler below as the only writer of the input
-- buffer. Actions that persist (mode, grants) go to Python via kn9t.notify;
-- navigation stays local in V.

kn9t.on_key("m", function()
    if V.adding then return false end
    kn9t.notify({ event = "cycle_mode" })
    return true
end)

kn9t.on_key("a", function()
    if V.adding then return false end
    V.adding = true
    V.input = ""
    return true
end)

-- `d` and `x` are the same action.
local function delete_grant()
    if V.cursor >= 0 and V.cursor < #V.grants then
        kn9t.notify({ event = "delete_grant", index = V.cursor })
    end
    return true
end

kn9t.on_key("d", function() if V.adding then return false end return delete_grant() end)
kn9t.on_key("x", function() if V.adding then return false end return delete_grant() end)

local function move(delta)
    local n = #V.grants
    if n == 0 then return end
    V.cursor = math.max(0, math.min(n - 1, V.cursor + delta))
end

kn9t.on_key("j", function() if V.adding then return false end move(1) return true end)
kn9t.on_key("k", function() if V.adding then return false end move(-1) return true end)
kn9t.on_key("Down", function() if V.adding then return false end move(1) return true end)
kn9t.on_key("Up", function() if V.adding then return false end move(-1) return true end)

kn9t.on_key("Escape", function()
    -- Clear a half-typed grant, then let Esc release focus.
    if V.adding then
        V.adding = false
        V.input = ""
    end
    return false
end)

kn9t.on_key("Enter", function()
    if not V.adding or V.input == "" then return false end
    kn9t.notify({ event = "add_grant", pattern = V.input })
    V.adding = false
    V.input = ""
    return true
end)

kn9t.on_key("Backspace", function()
    if not V.adding then return false end
    V.input = string.sub(V.input, 1, -2)
    return true
end)

-- One handler for every printable character, Space included. It owns the
-- keystroke only while a grant is being typed; otherwise the character falls
-- through to the shortcuts above or to the host.
kn9t.on_text(function(ch)
    if not V.adding then return false end
    V.input = V.input .. ch
    return true
end)

function render(s)
    on_state(s)
    local out = {}
    local C = {
        accent = "cyan",
        dim = "darkgray",
        normal = "green",
        yolo = "yellow",
        ask_all = "magenta",
        allow = "green",
        deny = "lightred",
        ask = "yellow",
    }
    
    -- Mode selector
    local mode_spans = {}
    table.insert(mode_spans, { text = "Mode: ", fg = C.dim })
    for _, m in ipairs({"normal", "yolo", "ask_all"}) do
        if V.mode == m then
            table.insert(mode_spans, { text = "[", fg = C[m] })
            table.insert(mode_spans, { text = m, fg = C[m], bold = true })
            table.insert(mode_spans, { text = "] ", fg = C[m] })
        else
            table.insert(mode_spans, { text = m .. " ", fg = C.dim })
        end
    end
    table.insert(out, { type = "text", spans = mode_spans, size = { fixed = 1 } })
    
    -- Separator
    table.insert(out, { type = "text", content = string.rep("─", 40), fg = C.dim, size = { fixed = 1 } })
    
    -- Recent decisions
    table.insert(out, { type = "text", content = "Recent:", fg = C.dim, size = { fixed = 1 } })
    if #V.recent == 0 then
        table.insert(out, { type = "text", content = "  (none)", fg = C.dim, size = { fixed = 1 } })
    else
        for i = #V.recent, 1, -1 do
            local d = V.recent[i]
            local icon = "✓"
            local col = C.allow
            if d.result == "deny" then icon = "✗"; col = C.deny
            elseif d.result == "ask" then icon = "?"; col = C.ask end
            local spans = {
                { text = icon .. " ", fg = col },
                { text = d.cmd, fg = "white" },
            }
            if d.reason ~= "" then
                table.insert(spans, { text = " (" .. d.reason .. ")", fg = C.dim })
            end
            table.insert(out, { type = "text", spans = spans, size = { fixed = 1 } })
        end
    end
    
    -- Separator
    table.insert(out, { type = "text", content = string.rep("─", 40), fg = C.dim, size = { fixed = 1 } })
    
    -- Grants list
    table.insert(out, { type = "text", content = "Grants (always allow):", fg = C.dim, size = { fixed = 1 } })
    if #V.grants == 0 and not V.adding then
        table.insert(out, { type = "text", content = "  (none) - press 'a' to add", fg = C.dim, size = { fixed = 1 } })
    else
        for i, g in ipairs(V.grants) do
            local prefix = (i - 1 == V.cursor) and "> " or "  "
            local fg = (i - 1 == V.cursor) and C.accent or "white"
            table.insert(out, { type = "text", content = prefix .. g, fg = fg, size = { fixed = 1 } })
        end
    end
    
    -- Input line for adding
    if V.adding then
        local spans = {
            { text = "  + ", fg = C.accent },
            { text = V.input, fg = "white" },
            { text = "█", fg = C.accent },
        }
        table.insert(out, { type = "text", spans = spans, size = { fixed = 1 } })
    end
    
    -- Spacer
    table.insert(out, { type = "spacer", size = { flex = 1 } })
    
    -- Help bar
    local help_spans = {}
    if V.adding then
        table.insert(help_spans, { text = "[Enter]", fg = C.accent })
        table.insert(help_spans, { text = " save  ", fg = C.dim })
        table.insert(help_spans, { text = "[Bksp]", fg = C.accent })
        table.insert(help_spans, { text = " clear", fg = C.dim })
    else
        table.insert(help_spans, { text = "[m]", fg = C.accent })
        table.insert(help_spans, { text = " mode  ", fg = C.dim })
        table.insert(help_spans, { text = "[a]", fg = C.accent })
        table.insert(help_spans, { text = " add  ", fg = C.dim })
        table.insert(help_spans, { text = "[d]", fg = C.accent })
        table.insert(help_spans, { text = " del  ", fg = C.dim })
        table.insert(help_spans, { text = "[↑↓]", fg = C.accent })
        table.insert(help_spans, { text = " nav", fg = C.dim })
    end
    table.insert(out, { type = "text", spans = help_spans, size = { fixed = 1 } })
    
    return { type = "split", direction = "vertical", children = out }
end
'''

_request_id = 0

def send_ui_state():
    """Push current state to TUI."""
    global _request_id
    if not session_id:
        return
    _request_id += 1
    write_msg({
        "t": "request",
        "id": _request_id,
        "op": "ui_set_state",
        "payload": {
            "session": session_id,
            "state": {
                "mode": state.mode,
                "grants": state.grants,
                "recent": [asdict(d) for d in state.recent[-MAX_RECENT:]],
                "cursor": state.cursor,
            }
        }
    })

def register_ui():
    """Register Lua UI with TUI."""
    global _request_id
    if not session_id:
        return
    _request_id += 1
    write_msg({
        "t": "request",
        "id": _request_id,
        "op": "ui_register_lua",
        "payload": {
            "session": session_id,
            "source": UI_LUA,
            "placement": "main",
            "title": "Policy",
        }
    })

# ══════════════════════════════════════════════════════════════════════════════
# Protocol
# ══════════════════════════════════════════════════════════════════════════════

def log(msg: str):
    print(msg, file=sys.stderr)

def read_msg():
    line = sys.stdin.readline()
    return json.loads(line) if line else None

def write_msg(msg):
    sys.stdout.write(json.dumps(msg, separators=(",", ":")) + "\n")
    sys.stdout.flush()

def run():
    global session_id
    
    load_grants()
    ui_registered = False
    
    # Handshake
    hello = read_msg()
    if not hello or hello.get("t") != "hello":
        return
    
    log(f"Connected to kn9t {hello.get('kn9t', '?')}")
    
    write_msg({
        "t": "hello",
        "name": "kn9t-policy",
        "capabilities": [],
        "hooks": ["before_tool_call", "get_steering"],
        "tools": [],
        "subscriptions": ["ui_interaction"],
    })
    
    # Main loop
    while True:
        msg = read_msg()
        if not msg:
            break
        
        t = msg.get("t", "")
        
        if t == "shutdown":
            break
        
        elif t == "hook" and msg.get("hook") == "before_tool_call":
            hook_id = msg.get("id", 0)
            payload = msg.get("payload", {})
            
            # Get session_id from hook payload (first time we see it)
            hook_session = payload.get("session_id")
            if hook_session and not session_id:
                session_id = hook_session
                log(f"Got session_id: {session_id}")
            
            # Register UI on first hook (now we have session_id)
            if session_id and not ui_registered:
                register_ui()
                send_ui_state()
                ui_registered = True
            
            tool = payload.get("tool", "")
            args = payload.get("args", {})
            cwd = payload.get("cwd", "")
            
            # Check policy
            result = check(tool, args, cwd)
            
            # Record decision
            cmd = args.get("cmd", args.get("command", tool))[:50]  # Truncate
            state.recent.append(Decision(
                tool=tool,
                cmd=cmd,
                result=result["action"],
                reason=result.get("reason", "")[:30]
            ))
            if len(state.recent) > MAX_RECENT:
                state.recent = state.recent[-MAX_RECENT:]
            
            send_ui_state()
            write_msg({"t": "result", "id": hook_id, **result})
        
        elif t == "hook":
            write_msg({"t": "result", "id": msg.get("id", 0), "action": "allow"})
        
        elif t == "event" and msg.get("kind") == "ui_interaction":
            handle_ui_event(msg)


def handle_ui_event(msg: dict):
    """Handle UI interaction events from kn9t.notify() calls."""
    event = msg.get("event", "")
    data = msg.get("data", {})
    
    log(f"UI event: {event} data={data}")
    
    if event == "cycle_mode":
        modes = ["normal", "yolo", "ask_all"]
        try:
            idx = modes.index(state.mode)
            state.mode = modes[(idx + 1) % len(modes)]
        except ValueError:
            state.mode = "normal"
        save_grants()
        send_ui_state()
    
    elif event == "add_grant":
        pattern = data.get("pattern", "")
        if pattern and pattern not in state.grants:
            state.grants.append(pattern)
            save_grants()
            send_ui_state()
    
    elif event == "delete_grant":
        index = data.get("index", -1)
        if 0 <= index < len(state.grants):
            state.grants.pop(index)
            save_grants()
            send_ui_state()


if __name__ == "__main__":
    run()
