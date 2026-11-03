import json
import os
import tempfile
import threading
import unittest
import xml.etree.ElementTree as ET

from xml2json.core import Cancelled, convert, count_lines, guess_record_tag
from xml2json.cli import main as cli_main

SAMPLE = """<?xml version="1.0" encoding="UTF-8"?>
<p:catalog xmlns:p="urn:parts">
  <p:header created="2026-01-01"/>
  <p:parts>
    <p:part id="00123" p:code=" A-B.45LMS">
      <p:name>Bolt</p:name>
    </p:part>
    <p:part id="2">
      <p:name>Nut</p:name>
      <p:name>Écrou</p:name>
    </p:part>
  </p:parts>
</p:catalog>
"""


class Base(unittest.TestCase):
    def setUp(self) -> None:
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)

    def path(self, name: str) -> str:
        return os.path.join(self.dir.name, name)

    def write(self, name: str, text: str, encoding: str = "utf-8") -> str:
        path = self.path(name)
        with open(path, "w", encoding=encoding, newline="") as handle:
            handle.write(text)
        return path


class ConvertTests(Base):
    def test_mapping(self) -> None:
        source = self.write("in.xml", SAMPLE)
        out = self.path("out.json")
        self.assertEqual(convert(source, out), 2)
        with open(out, encoding="utf-8") as handle:
            records = json.load(handle)
        self.assertEqual(
            records,
            [
                {"id": "00123", "code": " A-B.45LMS", "name": [{"#text": "Bolt"}]},
                {"id": "2", "name": [{"#text": "Nut"}, {"#text": "Écrou"}]},
            ],
        )
        self.assertFalse(os.path.exists(out + ".tmp"))

    def test_ndjson(self) -> None:
        source = self.write("in.xml", SAMPLE)
        out = self.path("out.ndjson")
        convert(source, out, ndjson=True)
        with open(out, encoding="utf-8") as handle:
            lines = handle.read().splitlines()
        self.assertEqual(len(lines), 2)
        self.assertEqual(json.loads(lines[0])["id"], "00123")

    def test_non_utf8_declared_encoding(self) -> None:
        text = SAMPLE.replace('encoding="UTF-8"', 'encoding="ISO-8859-1"')
        source = self.write("latin1.xml", text, encoding="latin-1")
        out = self.path("out.json")
        convert(source, out)
        with open(out, encoding="utf-8") as handle:
            self.assertEqual(json.load(handle)[1]["name"][1]["#text"], "Écrou")

    def test_attribute_child_clash(self) -> None:
        source = self.write("in.xml", '<r><part name="x"><name/></part></r>')
        out = self.path("out.json")
        with self.assertRaisesRegex(ValueError, "attribute and a child"):
            convert(source, out)
        self.assertFalse(os.path.exists(out))
        self.assertFalse(os.path.exists(out + ".tmp"))

    def test_no_records_keeps_previous_output(self) -> None:
        source = self.write("in.xml", "<r><item/></r>")
        out = self.write("out.json", "previous good result")
        with self.assertRaisesRegex(ValueError, "no <part> elements"):
            convert(source, out)
        with open(out, encoding="utf-8") as handle:
            self.assertEqual(handle.read(), "previous good result")
        self.assertFalse(os.path.exists(out + ".tmp"))

    def test_parse_error_cleans_up(self) -> None:
        source = self.write("in.xml", "<r><part id='1'/><part>")
        out = self.path("out.json")
        with self.assertRaises(ET.ParseError):
            convert(source, out)
        self.assertFalse(os.path.exists(out))
        self.assertFalse(os.path.exists(out + ".tmp"))

    def test_cancel_cleans_up(self) -> None:
        source = self.write("in.xml", "<r>" + "<part/>" * 50_000 + "</r>")
        out = self.path("out.json")
        cancel = threading.Event()

        def progress(_bytes: int, records: int) -> None:
            if records:
                cancel.set()

        with self.assertRaises(Cancelled):
            convert(source, out, progress=progress, cancel=cancel)
        self.assertFalse(os.path.exists(out))
        self.assertFalse(os.path.exists(out + ".tmp"))

    def test_cancel_works_when_tag_matches_nothing(self) -> None:
        source = self.write("in.xml", "<r>" + "<item/>" * 50_000 + "</r>")
        cancel = threading.Event()
        cancel.set()
        with self.assertRaises(Cancelled):
            convert(source, self.path("out.json"), cancel=cancel)

    def test_progress_reaches_file_size(self) -> None:
        source = self.write("in.xml", SAMPLE)
        seen: list[tuple[int, int]] = []
        convert(source, self.path("out.json"), progress=lambda b, r: seen.append((b, r)))
        self.assertEqual(seen[-1], (os.path.getsize(source), 2))


class HelperTests(Base):
    def test_count_lines(self) -> None:
        self.assertEqual(count_lines(self.write("a", "")), 0)
        self.assertEqual(count_lines(self.write("b", "one")), 1)
        self.assertEqual(count_lines(self.write("c", "one\n")), 1)
        self.assertEqual(count_lines(self.write("d", "one\ntwo")), 2)
        self.assertEqual(count_lines(self.write("e", "one\r\ntwo\r\n")), 2)

    def test_guess_record_tag(self) -> None:
        self.assertEqual(guess_record_tag(self.write("a.xml", SAMPLE)), "part")
        flat = "<parts><part/><part/></parts>"
        self.assertEqual(guess_record_tag(self.write("b.xml", flat)), "part")
        self.assertIsNone(guess_record_tag(self.write("c.xml", "<only/>")))
        self.assertIsNone(guess_record_tag(self.write("d.xml", "not xml")))


class CliTests(Base):
    def test_cli_success_and_failure(self) -> None:
        source = self.write("in.xml", SAMPLE)
        out = self.path("out.json")
        self.assertEqual(cli_main([source, out]), 0)
        self.assertTrue(os.path.exists(out))
        self.assertEqual(cli_main([source, out, "--tag", "missing"]), 1)


if __name__ == "__main__":
    unittest.main()
