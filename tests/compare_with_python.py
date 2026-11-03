#!/usr/bin/env python3
"""Check that the Rust converter matches the Python reference byte for byte.

Usage:
    cargo build --release
    python3 tests/compare_with_python.py [--random N] [--seed S]

For every case both implementations convert the same file:
    - compact output must be identical to Python's output,
    - --pretty output must equal json.dumps(records, indent=2) + "\n",
    - --ndjson output must be identical to Python's --ndjson output,
    - when Python fails, Rust must fail too (messages may differ),
    - the parallel scanner must find exactly what the sequential one finds,
      at chunk sizes down to 1 byte (forcing every resync path).
"""

from __future__ import annotations

import argparse
import json
import os
import random
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "python"))
from xml2json.core import convert as python_convert  # noqa: E402

BINARY = os.path.join(ROOT, "target", "release", "tome")

NS = '<p:catalog xmlns:p="urn:p" xmlns="urn:d"><p:parts>{}</p:parts></p:catalog>'

CASES: list[tuple[str, bytes, str]] = [
    ("namespaces", NS.format(
        '<p:part id="00123" p:code=" A-B.45LMS"><p:name>Bolt</p:name></p:part>'
        '<part id="2"><name>Nut</name><name>Écrou</name></part>').encode(), "part"),
    ("empty record", b"<r><part/><part></part><part>  </part></r>", "part"),
    ("escapes in text", '<r><part>a "q" \\ b\tc\nd</part></r>'.encode(), "part"),
    ("crlf text", b"<r><part>a\r\nb\rc</part>\r\n</r>", "part"),
    ("attribute whitespace", b'<r><part a="x\ty\nz\r\nw" b="&#10;&#9;keep" c=\'single "q"\' d="1 &gt; 0"/></r>', "part"),
    ("cdata comments pi", b"<r><part>one<!-- c --> two<![CDATA[ <three> & ]]>four<?pi x?>five<child/>tail</part></r>", "part"),
    ("tail text ignored", b"<r><part>head<a>x</a>middle<b/>end</part>junk-ignored-outside</r>", "part"),
    ("references", b"<r><part v=\"&lt;&amp;&#65;&#x1F600;\">&quot;&apos;&#169;&#xe9;</part></r>", "part"),
    ("dtd entities", b'<?xml version="1.0"?>\n<!DOCTYPE r [\n<!ENTITY co "ACME &amp; Sons">\n<!ENTITY nested "[&co;]">\n]>\n<r><part maker="&co;">&nested;</part></r>', "part"),
    ("xmlns and xml:lang", b'<r xmlns:a="urn:a" xmlns:b="urn:b"><part xml:lang="nl" a:id="1" b:id="2" id="3"/></r>', "part"),
    ("nested records", b"<r><part id='outer'><part id='inner'><part id='deepest'/></part></part><part id='next'/></r>", "part"),
    ("mixed depths", b"<r><group><part id='1'/></group><part id='2'/><x><y><part id='3'/></y></x></r>", "part"),
    ("unicode", "<r><part naam='Ωmega' emoji='😀'>中文   sep nbsp </part></r>".encode(), "part"),
    ("utf-8 bom", b"\xef\xbb\xbf<?xml version='1.0' encoding='utf-8'?><r><part>bom</part></r>", "part"),
    ("iso-8859-1", '<?xml version="1.0" encoding="ISO-8859-1"?><r><part>Écrou \x85\x80</part></r>'.encode("latin-1"), "part"),
    ("utf-16 bom", '<?xml version="1.0" encoding="UTF-16"?><r><part>wide é</part></r>'.encode("utf-16"), "part"),
    ("record is root", b"<part a='1'><b>x</b><b>y</b></part>", "part"),
    ("names with dots", b"<r><part><my.name-x>1</my.name-x><_u>2</_u></part></r>", "part"),
    ("whitespace kinds", b"<r><part>\n\t  padded \t\n</part><part>\n</part></r>", "part"),
    ("char ref cr", b"<r><part>a&#13;b&#xD;&#x0A;c</part></r>", "part"),
    ("del char", b"<r><part>a\x7fb</part></r>", "part"),
    ("gt in attribute", b"<r><part a='x>y' b=\"it's\">t</part></r>", "part"),
    ("other tag", b"<r><item k='v'/><item k='w'><sub/></item></r>", "item"),
    ("big text", b"<r><part>" + b"x" * 200_000 + b"</part></r>", "part"),
    ("many children", b"<r><part>" + b"".join(b"<c i='%d'/>" % i for i in range(3000)) + b"</part></r>", "part"),
    ("markup-like text in comments", b"<r><!-- <part id='fake'/> --><part id='1'><![CDATA[<part id='fake2'/>]]></part><?pi <part/> ?><part a='&lt;part/>'/></r>", "part"),
    ("doctype with brackets", b"<!DOCTYPE r [<!ENTITY e '<!-- not a comment -->x'>]><r><part/></r>", "part"),
    ("deep nesting", b"<r>" + b"<g>" * 300 + b"<part><x/></part>" + b"</g>" * 300 + b"</r>", "part"),
    ("record spans everything", b"<r><part>" + b"".join(b"<c>%d</c>" % i for i in range(500)) + b"</part></r>", "part"),
    ("empty root", b"<r/>", "part"),
    ("root is empty record", b"<part a='1'/>", "part"),
    ("comment after root", b"<r><part/></r>\n<!-- bye -->\n<?pi end?>\n", "part"),
    # Failures: both sides must reject these.
    ("element after root", b"<r><part/></r><part/>", "part"),
    ("close without open", b"<r><part/></r></x>", "part"),
    ("stray close inside", b"<r><part/></x></r>", "part"),
    ("truncated", b"<r><part id='1'/><part>", "part"),
    ("mismatched", b"<r><part></prt></r>", "part"),
    ("junk after root", b"<r><part/></r><r/>", "part"),
    ("text after root", b"<r><part/></r>text", "part"),
    ("undefined entity", b"<r><part>&nope;</part></r>", "part"),
    ("bad char ref", b"<r><part>&#1;</part></r>", "part"),
    ("raw control char", b"<r><part>a\x01b</part></r>", "part"),
    ("attribute child clash", b"<r><part name='x'><name/></part></r>", "part"),
    ("no records", b"<r><item/></r>", "part"),
    ("duplicate attribute", b"<r><part a='1' a='2'/></r>", "part"),
    ("cdata end in text", b"<r><part>a ]]> b</part></r>", "part"),
    ("double dash comment", b"<r><part><!-- a -- b --></part></r>", "part"),
    ("lt in attribute", b"<r><part a='<'/></r>", "part"),
    ("empty file", b"", "part"),
    ("invalid utf-8", b"<r><part>\xff</part></r>", "part"),
    ("missing attr quote", b"<r><part a=1/></r>", "part"),
    ("no space between attrs", b"<r><part a='1'b='2'/></r>", "part"),
    ("bare ampersand", b"<r><part>a & b</part></r>", "part"),
    ("xml decl not at start", b"<r><?xml version='1.0'?><part/></r>", "part"),
]

# Known, documented differences: Python rejects these, Rust accepts them.
KNOWN_LENIENT = {
    "undeclared prefix": (b"<r><x:part/></r>", "part"),
}

TEXT_POOL = ["plain", "  spaced  ", "a&amp;b", "&lt;tag&gt;", "quote\"s", "it's", "tab\there",
             "line\nbreak", "crlf\r\nx", "é ü ß", "中文", "😀", "back\\slash", "&#38;", "&#x3C;",
             "", "   ", "<![CDATA[cdata <x> & ]]>", "<!-- comment -->", "00123", " A-B.45LMS"]
ATTR_POOL = [v.replace('"', "&quot;") for v in TEXT_POOL if "<" not in v] + ["x&lt;y", "&quot;q&quot;", "tab\tnl\ncr\r\nend", "&#10;kept"]
# "code" doubles as a child name now and then, to exercise the clash error.
NAMES = ["part", "name", "price", "a", "b", "p:item", "q:name", "x.y", "long-name_1"] * 6 + ["code"]


def random_element(rng: random.Random, depth: int, force: str | None = None) -> str:
    name = force or rng.choice(NAMES)
    attrs = []
    used = set()
    for _ in range(rng.randrange(0, 4)):
        attr = rng.choice(["id", "code", "p:code", "q:code", "lang", "v"])
        if attr in used:
            continue
        used.add(attr)
        value = rng.choice(ATTR_POOL)
        attrs.append(f' {attr}="{value}"')
    if depth <= 0 or rng.random() < 0.25:
        if rng.random() < 0.5:
            return f"<{name}{''.join(attrs)}/>"
        return f"<{name}{''.join(attrs)}>{rng.choice(TEXT_POOL)}</{name}>"
    parts = []
    for _ in range(rng.randrange(0, 5)):
        if rng.random() < 0.4:
            parts.append(rng.choice(TEXT_POOL))
        parts.append(random_element(rng, depth - 1))
    return f"<{name}{''.join(attrs)}>{''.join(parts)}</{name}>"


def random_document(rng: random.Random) -> bytes:
    records = "".join(random_element(rng, 3, force=rng.choice(["part", "p:part"])) + rng.choice(["", "\n", "  "])
                      for _ in range(rng.randrange(1, 8)))
    noise = random_element(rng, 1, force="noise") if rng.random() < 0.5 else ""
    return f'<root xmlns:p="urn:p" xmlns:q="urn:q">{noise}{records}</root>'.encode()


def run_python(path: str, out: str, tag: str, ndjson: bool) -> tuple[bool, bytes | str]:
    try:
        python_convert(path, out, tag=tag, ndjson=ndjson)
    except (ET.ParseError, ValueError, OSError, UnicodeError, LookupError) as e:
        return False, str(e)
    with open(out, "rb") as f:
        return True, f.read()


def run_rust(path: str, out: str, tag: str, flag: str | None) -> tuple[bool, bytes | str]:
    args = [BINARY, path, out, "--tag", tag, "--quiet"] + ([flag] if flag else [])
    proc = subprocess.run(args, capture_output=True, text=True)
    if proc.returncode != 0:
        return False, proc.stderr.strip()
    with open(out, "rb") as f:
        return True, f.read()


def compare(name: str, data: bytes, tag: str, workdir: str) -> list[str]:
    path = os.path.join(workdir, "in.xml")
    with open(path, "wb") as f:
        f.write(data)
    problems = []
    check = subprocess.run([BINARY, "--check-scan", path, tag], capture_output=True, text=True)
    if check.returncode != 0:
        problems.append(f"{name}: parallel scanner disagrees with sequential scanner: {check.stdout.strip()}")
    py_ok, py = run_python(path, os.path.join(workdir, "py.json"), tag, False)
    rs_ok, rs = run_rust(path, os.path.join(workdir, "rs.json"), tag, None)
    if py_ok != rs_ok:
        problems.append(f"{name}: python {'ok' if py_ok else 'failed: ' + str(py)}, rust {'ok' if rs_ok else 'failed: ' + str(rs)}")
        return problems
    if not py_ok:
        return problems
    if py != rs:
        problems.append(f"{name}: compact output differs\n  python: {py[:400]!r}\n  rust:   {rs[:400]!r}")
    expected_pretty = (json.dumps(json.loads(py), indent=2, ensure_ascii=False) + "\n").encode()
    ok, pretty = run_rust(path, os.path.join(workdir, "rs.pretty.json"), tag, "--pretty")
    if not ok or pretty != expected_pretty:
        problems.append(f"{name}: pretty output differs\n  expected: {expected_pretty[:400]!r}\n  rust:     {str(pretty)[:400]!r}")
    _, py_nd = run_python(path, os.path.join(workdir, "py.ndjson"), tag, True)
    ok, rs_nd = run_rust(path, os.path.join(workdir, "rs.ndjson"), tag, "--ndjson")
    if not ok or py_nd != rs_nd:
        problems.append(f"{name}: ndjson output differs\n  python: {py_nd[:400]!r}\n  rust:   {str(rs_nd)[:400]!r}")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--random", type=int, default=500, help="number of random documents")
    parser.add_argument("--seed", type=int, default=1)
    args = parser.parse_args()
    if not os.path.exists(BINARY):
        print(f"build first: {BINARY} not found", file=sys.stderr)
        return 2

    problems: list[str] = []
    with tempfile.TemporaryDirectory() as workdir:
        for name, data, tag in CASES:
            problems += compare(name, data, tag, workdir)
        for name, (data, tag) in KNOWN_LENIENT.items():
            path = os.path.join(workdir, "in.xml")
            with open(path, "wb") as f:
                f.write(data)
            py_ok, _ = run_python(path, os.path.join(workdir, "py.json"), tag, False)
            rs_ok, _ = run_rust(path, os.path.join(workdir, "rs.json"), tag, None)
            print(f"known difference '{name}': python {'accepts' if py_ok else 'rejects'}, rust {'accepts' if rs_ok else 'rejects'}")
        rng = random.Random(args.seed)
        for i in range(args.random):
            problems += compare(f"random #{i} (seed {args.seed})", random_document(rng), "part", workdir)

    total = len(CASES) + args.random
    for p in problems:
        print("FAIL", p)
    print(f"{total - len(problems)}/{total} cases match" if problems else f"all {total} cases match")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
