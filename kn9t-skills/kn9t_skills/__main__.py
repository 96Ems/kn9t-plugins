"""Entry point for kn9t-skills plugin."""

from kn9t_skills.plugin import Plugin


def main() -> None:
    """Run the plugin."""
    plugin = Plugin.from_config()
    plugin.run()


if __name__ == "__main__":
    main()
