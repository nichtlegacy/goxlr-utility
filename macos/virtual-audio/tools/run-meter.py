#!/usr/bin/env python3
"""Launch one meter instance and bound its Launch Services wait."""

import argparse
import subprocess
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", type=Path)
    parser.add_argument("seconds", type=float)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    if not 1 <= args.seconds <= 60:
        parser.error("seconds must be between 1 and 60")

    app = args.app.resolve()
    if not (app / "Contents/MacOS/meter").is_file():
        parser.error(f"meter executable missing in {app}")
    directory = args.directory.resolve()
    directory.mkdir(parents=True, exist_ok=False)
    log = directory / "run.txt"
    command = [
        "/usr/bin/open", "-g", "-n", "-W", "--stdout", str(log), "--stderr", str(log),
        str(app), "--args", str(args.seconds), str(directory),
    ]
    try:
        subprocess.run(command, timeout=args.seconds + 20, check=True)
    except subprocess.TimeoutExpired:
        print("open -W timed out. The meter has its own watchdog.")
    output = log.read_text() if log.exists() else ""
    print(output, end="")
    if "Capture complete" not in output:
        parser.exit(1, f"Incomplete capture. See {log}\n")


if __name__ == "__main__":
    main()
