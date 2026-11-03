"""Command line interface.

Usage:
    python3 -m xml2json parts.xml parts.json
    python3 -m xml2json parts.xml parts.ndjson --ndjson
    python3 -m xml2json parts.xml parts.json --tag part
"""

from __future__ import annotations

import argparse
import sys
import xml.etree.ElementTree as ET

from .core import convert


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="xml2json",
        description="Stream large XML files to JSON with roughly constant memory use.",
    )
    parser.add_argument("input", help="input XML file")
    parser.add_argument("output", help="output JSON file")
    parser.add_argument(
        "--tag", default="part", help="element to emit (default: part)"
    )
    parser.add_argument(
        "--ndjson",
        action="store_true",
        help="write one JSON object per line instead of one array",
    )
    args = parser.parse_args(argv)

    next_report = 10_000

    def report(_bytes_read: int, count: int) -> None:
        nonlocal next_report
        if count >= next_report:
            print(f"{next_report:,} records", file=sys.stderr)
            next_report += 10_000

    try:
        count = convert(args.input, args.output, args.tag, args.ndjson, report)
    except (ET.ParseError, OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("cancelled", file=sys.stderr)
        return 130

    print(f"wrote {count:,} records to {args.output}", file=sys.stderr)
    return 0
