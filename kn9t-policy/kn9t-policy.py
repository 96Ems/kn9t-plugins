#!/usr/bin/env python3
"""kn9t-policy: Minimal approval plugin.

ALLOW everything by default. ASK only for truly destructive commands.
No external config file, no friction.
"""

import json
import sys
import re
import fnmatch


# ══════════════════════════════════════════════════════════════════════════════
# DESTRUCTIVE ONLY — everything else is auto-allowed
# ══════════════════════════════════════════════════════════════════════════════

ASK = [
    # Git destructive (loses commits/changes)
    "git checkout*", "git reset*", "git clean*",
    "git rebase*", "git merge*", "git cherry-pick*", "git revert*",
    "git stash drop*", "git stash pop*", "git stash clear*",
    "git branch -D*", "git branch -d*", "git branch --delete*",
    "git push --force*", "git push -f*",
    
    # File deletion
    "rm *", "rm", "rmdir *", "del *", "del", "Remove-Item*",
    "shred*", "unlink *",
    
    # Publishing (irreversible)
    "npm publish*", "cargo publish*", "twine upload*", "docker push*",
    
    # System
    "sudo *", "su *",
    "kill *", "killall *", "pkill *",
    
    # Disk
    "fdisk*", "parted*", "mkfs*", "format *",
]

DENY = [
    "rm -rf /*", "rm -rf /", "rm -rf ~*",
    "mkfs*", "shutdown*", "reboot*", "poweroff*",
    ":(){ :|:& };:",
    "dd*of=/dev/sd*",
]


def matches(value, patterns):
    return any(fnmatch.fnmatch(value, p) for p in patterns)


def split_commands(cmd):
    return [p.strip() for p in re.split(r"\s*(?:;|&&|\|\||\|)\s*", cmd) if p.strip()]


def extract_inner(part):
    """Extract inner command from wrappers like cmd.exe /c '...'"""
    # cmd.exe /c "..."
    m = re.match(r'cmd\.exe\s+/c\s+["\']([^"\']+)["\']', part, re.I)
    if m:
        return m.group(1)
    # cmd.exe /c ...
    m = re.match(r'cmd\.exe\s+/c\s+(.+)', part, re.I)
    if m:
        return m.group(1)
    return part


def check_part(part):
    """Check a single command part. Returns: True=allow, None=ask, False=deny"""
    inner = extract_inner(part)
    if matches(inner, DENY):
        return False
    if matches(inner, ASK):
        return None
    return True


def check(tool, args, cwd):
    if tool == "bash":
        cmd = args.get("cmd", args.get("command", ""))
        for part in split_commands(cmd):
            result = check_part(part)
            if result is None:
                return "ask"
            if result is False:
                return {"action": "deny", "reason": f"Blocked: {part}"}
    return "allow"


# ══════════════════════════════════════════════════════════════════════════════
# Plugin protocol
# ══════════════════════════════════════════════════════════════════════════════

def read_msg():
    line = sys.stdin.readline()
    return json.loads(line) if line else None


def write_msg(msg):
    sys.stdout.write(json.dumps(msg, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def run():
    hello = read_msg()
    if not hello or hello.get("t") != "hello":
        return

    write_msg({
        "t": "hello",
        "name": "kn9t-policy",
        "capabilities": [],
        "hooks": ["before_tool_call"],
        "tools": [],
    })

    while True:
        msg = read_msg()
        if not msg:
            break
        if msg.get("t") == "shutdown":
            break

        if msg.get("t") == "hook" and msg.get("hook") == "before_tool_call":
            hook_id = msg.get("id", 0)
            payload = msg.get("payload", {})
            tool = payload.get("tool", "")
            args = payload.get("args", {})
            cwd = payload.get("cwd", "")

            r = check(tool, args, cwd)
            if r == "allow":
                result = {"action": "allow"}
            elif r == "ask":
                result = {"action": "ask", "reason": "Destructive command"}
            elif isinstance(r, dict):
                result = r
            else:
                result = {"action": "allow"}

            write_msg({"t": "result", "id": hook_id, **result})

        elif msg.get("t") == "hook":
            write_msg({"t": "result", "id": msg.get("id", 0), "action": "allow"})


if __name__ == "__main__":
    run()
