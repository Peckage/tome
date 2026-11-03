"""Streaming XML to JSON conversion with roughly constant memory use.

Mapping:
    - Attributes become string keys. Values are not coerced, so "00123" and
      " A-B.45LMS" reach the JSON unchanged.
    - Child elements always become arrays, even when there is only one, so
      consumers never have to handle both an object and a list.
    - Non-whitespace element text is stored under "#text". Text between child
      elements (mixed content) is not kept.
    - Namespace prefixes are removed from element and attribute names.
"""

from __future__ import annotations

import json
import os
import threading
import xml.etree.ElementTree as ET
from collections import Counter
from collections.abc import Callable, Iterator
from typing import Any, BinaryIO

CHUNK_SIZE = 8 * 1024 * 1024

# Called with (bytes_read, records_written). Called often, so keep it cheap.
ProgressCallback = Callable[[int, int], None]


class Cancelled(Exception):
    """Raised inside a conversion when the caller sets the cancel event."""


def local_name(name: str) -> str:
    return name.rsplit("}", 1)[-1]


def count_lines(path: str, cancel: threading.Event | None = None) -> int:
    """Count lines by scanning raw bytes. Nothing is parsed or decoded.

    A final line without a trailing newline still counts as a line.
    """
    lines = 0
    last = b""
    with open(path, "rb") as source:
        while chunk := source.read(CHUNK_SIZE):
            if cancel is not None and cancel.is_set():
                raise Cancelled
            lines += chunk.count(b"\n")
            last = chunk[-1:]
    if last and last != b"\n":
        lines += 1
    return lines


def guess_record_tag(path: str, max_events: int = 5000) -> str | None:
    """Guess which element is the repeating record, from the start of the file.

    Picks the shallowest element name below the root that occurs more than
    once, so both <parts><part/>... and <catalog><parts><part/>... give "part".
    Only the first few thousand parse events are read, so this is quick on any
    file size. Returns None when there is nothing to suggest.
    """
    counts: dict[int, Counter[str]] = {}
    depth = 0
    try:
        with open(path, "rb") as source:
            events = ET.iterparse(source, events=("start", "end"))
            for seen, (event, element) in enumerate(events):
                if seen >= max_events:
                    break
                if event == "start":
                    depth += 1
                    if depth >= 2:
                        counts.setdefault(depth, Counter())[local_name(element.tag)] += 1
                else:
                    depth -= 1
    except ET.ParseError:
        pass
    for level in sorted(counts):
        name, seen = counts[level].most_common(1)[0]
        if seen > 1:
            return name
    if 2 in counts:
        return counts[2].most_common(1)[0][0]
    return None


def element_to_dict(element: ET.Element) -> dict[str, Any]:
    result: dict[str, Any] = {
        local_name(key): value for key, value in element.attrib.items()
    }

    text = element.text.strip() if element.text else ""
    if text:
        result["#text"] = text

    for child in element:
        key = local_name(child.tag)
        children = result.setdefault(key, [])
        # An attribute with the same name as a child would otherwise be lost
        # or crash on append, so fail with a message that names the conflict.
        if not isinstance(children, list):
            raise ValueError(
                f"<{local_name(element.tag)}> has both an attribute and a "
                f"child element named {key!r}"
            )
        children.append(element_to_dict(child))

    return result


class _CountingReader:
    """File wrapper that reports how many bytes the parser has consumed.

    The hook runs on every read, not every record, so progress and cancel
    still work when the tag matches nothing.
    """

    def __init__(self, raw: BinaryIO, on_read: Callable[[int], None]) -> None:
        self._raw = raw
        self._on_read = on_read
        self.bytes_read = 0

    def read(self, size: int = -1) -> bytes:
        data = self._raw.read(size)
        self.bytes_read += len(data)
        self._on_read(self.bytes_read)
        return data


def iter_records(
    source: str | BinaryIO | _CountingReader, tag: str
) -> Iterator[dict[str, Any]]:
    """Yield each matching element, releasing it from memory once converted.

    Clearing an element is not enough: its parent still holds a reference, so
    the tree keeps growing. Each finished element outside a record is therefore
    also removed from its parent, including unrelated siblings of the records.
    """
    ancestors: list[ET.Element] = []
    record_depth = 0

    for event, element in ET.iterparse(source, events=("start", "end")):
        is_record = local_name(element.tag) == tag

        if event == "start":
            ancestors.append(element)
            record_depth += is_record
            continue

        ancestors.pop()
        record_depth -= is_record

        if is_record and record_depth == 0:
            yield element_to_dict(element)

        if record_depth == 0:
            element.clear()
            if ancestors:
                ancestors[-1].remove(element)


def _write_records(
    records: Iterator[dict[str, Any]],
    path: str,
    ndjson: bool,
    on_record: Callable[[int], None],
) -> int:
    count = 0
    with open(path, "w", encoding="utf-8", newline="\n") as output:
        if not ndjson:
            output.write("[")
        for record in records:
            if not ndjson:
                output.write(",\n" if count else "\n")
            output.write(
                json.dumps(record, ensure_ascii=False, separators=(",", ":"))
            )
            if ndjson:
                output.write("\n")
            count += 1
            on_record(count)
        if not ndjson:
            output.write("\n]\n")
    return count


def convert(
    input_path: str,
    output_path: str,
    tag: str = "part",
    ndjson: bool = False,
    progress: ProgressCallback | None = None,
    cancel: threading.Event | None = None,
) -> int:
    """Convert input_path to output_path and return the number of records.

    Output goes to a temporary file first, so a failed or cancelled run never
    leaves a truncated file at output_path or replaces an earlier good result.

    Raises ET.ParseError, OSError, ValueError, or Cancelled.
    """
    temporary_path = f"{output_path}.tmp"
    written = 0

    def check(bytes_read: int) -> None:
        if cancel is not None and cancel.is_set():
            raise Cancelled
        if progress is not None:
            progress(bytes_read, written)

    def on_record(count: int) -> None:
        nonlocal written
        written = count

    try:
        with open(input_path, "rb") as raw:
            reader = _CountingReader(raw, check)
            count = _write_records(
                iter_records(reader, tag), temporary_path, ndjson, on_record
            )
            if progress is not None:
                progress(reader.bytes_read, count)

        if count == 0:
            raise ValueError(f"no <{tag}> elements found")
        os.replace(temporary_path, output_path)
    except BaseException:
        if os.path.exists(temporary_path):
            os.remove(temporary_path)
        raise
    return count
