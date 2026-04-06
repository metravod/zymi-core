"""CLI entry point for the ``zymi`` command installed via pip."""

import sys


def main() -> None:
    from zymi_core._zymi_core import cli_main

    cli_main(sys.argv)


if __name__ == "__main__":
    main()
