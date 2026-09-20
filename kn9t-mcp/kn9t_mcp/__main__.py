#!/usr/bin/env python3
"""Entry point for kn9t-mcp plugin.

Usage:
    python -m kn9t_mcp
    
Or if installed:
    kn9t-mcp

The plugin communicates with kn9t-server via stdin/stdout using the
kn9t plugin v2 protocol (newline-delimited JSON).
"""

import sys
from kn9t_mcp.plugin import Plugin


def main() -> None:
    """Run the kn9t-mcp plugin."""
    try:
        plugin = Plugin.from_config()
        plugin.run()
    except KeyboardInterrupt:
        sys.exit(0)
    except Exception as e:
        # Log to stderr (kn9t captures this for diagnostics)
        print(f"kn9t-mcp fatal error: {e}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
