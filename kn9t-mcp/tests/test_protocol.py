"""Tests for kn9t plugin v2 protocol handling."""

import json
import subprocess
import sys
from pathlib import Path


def test_hello_handshake():
    """Plugin responds to hello with valid declaration."""
    # Run plugin with mocked stdin
    plugin_path = Path(__file__).parent.parent / "kn9t_mcp"
    
    proc = subprocess.Popen(
        [sys.executable, "-m", "kn9t_mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        cwd=plugin_path.parent,
    )
    
    # Send hello
    hello = {"t": "hello", "proto": 1, "kn9t": "0.1.0"}
    proc.stdin.write(json.dumps(hello) + "\n")
    proc.stdin.flush()
    
    # Read response
    response_line = proc.stdout.readline()
    response = json.loads(response_line)
    
    # Verify
    assert response["t"] == "hello"
    assert response["name"] == "kn9t-mcp"
    assert "streaming" in response["capabilities"]
    assert "cancelable" in response["capabilities"]
    assert isinstance(response["tools"], list)
    
    # Shutdown
    proc.stdin.write('{"t":"shutdown"}\n')
    proc.stdin.flush()
    proc.wait(timeout=5)


def test_unknown_tool_returns_error():
    """Calling unknown tool returns is_error=true."""
    plugin_path = Path(__file__).parent.parent / "kn9t_mcp"
    
    proc = subprocess.Popen(
        [sys.executable, "-m", "kn9t_mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        cwd=plugin_path.parent,
    )
    
    # Handshake
    proc.stdin.write('{"t":"hello","proto":1,"kn9t":"0.1.0"}\n')
    proc.stdin.flush()
    proc.stdout.readline()  # discard hello response
    
    # Call unknown tool
    call = {
        "t": "hook",
        "id": 1,
        "hook": "tool_call",
        "payload": {"tool": "nonexistent_tool", "args": {}},
    }
    proc.stdin.write(json.dumps(call) + "\n")
    proc.stdin.flush()
    
    # Read response
    response_line = proc.stdout.readline()
    response = json.loads(response_line)
    
    assert response["t"] == "done"
    assert response["id"] == 1
    assert response["is_error"] is True
    assert "Unknown tool" in response["content"][0]["text"]
    
    # Shutdown
    proc.stdin.write('{"t":"shutdown"}\n')
    proc.stdin.flush()
    proc.wait(timeout=5)


def test_non_tool_hook_returns_keep():
    """Non-tool hooks return action=keep (pass-through)."""
    plugin_path = Path(__file__).parent.parent / "kn9t_mcp"
    
    proc = subprocess.Popen(
        [sys.executable, "-m", "kn9t_mcp"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        cwd=plugin_path.parent,
    )
    
    # Handshake
    proc.stdin.write('{"t":"hello","proto":1,"kn9t":"0.1.0"}\n')
    proc.stdin.flush()
    proc.stdout.readline()
    
    # Call before_request hook (which we don't handle)
    call = {
        "t": "hook",
        "id": 2,
        "hook": "before_request",
        "payload": {},
    }
    proc.stdin.write(json.dumps(call) + "\n")
    proc.stdin.flush()
    
    response_line = proc.stdout.readline()
    response = json.loads(response_line)
    
    assert response["t"] == "result"
    assert response["id"] == 2
    assert response["action"] == "keep"
    
    # Shutdown
    proc.stdin.write('{"t":"shutdown"}\n')
    proc.stdin.flush()
    proc.wait(timeout=5)
