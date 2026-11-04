# Tome

A text viewer and converter built for heavy files.

Tome opens files of hundreds of megabytes instantly, lets you scroll and search millions of lines or records, and converts XML and JSON to JSON, NDJSON, YAML, XML, CSV and TSV. It is a single native binary (Rust, egui) with a window and a command line. It started life as `xml2json`, and that converter is still its core.

On a 501 MB file with 2.6 million records (20-core i7-13650HX, file already in the page cache):

| Step | Time |
|---|---|
| Open, count lines, guess record tag | 0.12 s |
| Index every record (browse/scroll) | 0.12 s |
| Search all records | 0.3 s |
| Convert to compact JSON (483 MB) | 0.48 s |
| Convert to pretty JSON (951 MB) | 0.64 s |
| Previous Python version (compact) | 29.3 s |

The compact output is byte-for-byte identical to the Python version's.

## Install

```sh
./install.sh            # builds, copies the binary to ./bin, adds "Tome" to the app menu
./install.sh --remove   # removes the menu entry
```

The menu entry also shows up under "Open with" for text, XML and JSON files (it is not made the default app). Outside this folder the script writes only `~/.local/share/applications/io.github.peckage.Tome.desktop` (and removes the old `io.github.xml2json.Converter.desktop`), and it refreshes `mimeinfo.cache` in that folder.

## Text view

Any file opens in the text view: logs, CSV, minified JSON, source code, anything. The file is memory-mapped and its line starts are found on all cores, so nothing is loaded or rendered up front.

| 448 MB log, 5 million lines | Time |
|---|---|
| Open and index every line | 0.04 s |
| Find a word (all matches) | 0.02 s |
| Find with a regular expression | 0.02 s |

A 132 MB minified JSON file that is a single line scrolls just as smoothly, because only the part of each line that fits on screen is decoded.

- **Scroll** with the wheel (Shift+wheel or ←/→ scrolls sideways), PageUp/PageDown, Home/End or the scrollbar.
- **Select** lines by clicking, Shift+clicking or dragging, or with ↑/↓ (Shift extends). Ctrl+A selects everything.
- **Copy** with Ctrl+C or the right-click menu. Copies are capped at 64 MB.
- **Find** (Ctrl+F) highlights every match. Enter or F3 goes to the next one, Shift+F3 to the previous one. The search ignores case unless you type a capital. Turn on `.*` for regular expressions.
- **Go to line** with Ctrl+G.
- The toolbar shows the encoding guess (UTF-8, UTF-16, 8-bit or binary) and the line endings (LF or CRLF).

XML files also get the **Records** view below. Switch between the two with the tabs above the list.

## Records view (XML and JSON)

`bin/tome` (or `bin/tome any-file`, or drop a file on the window, or Ctrl+O).

XML and JSON files also get a Records view, which is where converting happens.

- **Opening** a file maps it into memory without loading or rendering it.
  - **XML:** the app suggests the record tag (the first repeating element). A tag you typed yourself is kept.
  - **JSON:** *Records at* picks the records automatically: the items of the root array, or of the biggest array under the root object. You can also type a key path such as `data.items`, `$` (the whole file is one record) or `lines` (one value per line).
  - **NDJSON and JSON Lines:** `.ndjson`/`.jsonl` files, and any file with several top-level values, give one record per value.
- **Records** are listed one per row. Click a record to expand it into a tree with one `key: value` per line, and click any branch to fold it. The tree hides the always-a-list wrapper to make it easier to read. The panel on the right shows the exact JSON the conversion writes for the selected record, with a Copy button.
- **Scrolling** works with the wheel, PageUp/PageDown, Home/End or the scrollbar, smoothly across millions of records. Only visible records are parsed.
- **Search** looks through every record's JSON in parallel. It ignores case unless the search text has capitals. **Go to #** jumps to a record number.
- **Convert** writes any of the formats below, with progress and Cancel. It asks before replacing a file. If you cancel, hit an error or close the window, the previous output stays untouched.

### Output formats

| Format | What you get |
|---|---|
| Pretty JSON | An indented array, exactly like `json.dumps(records, indent=2)` |
| Compact JSON | An array with one compact record per line |
| NDJSON | One compact record per line, no array |
| YAML | A sequence with one item per record. Strings are always double-quoted, so `"00123"`, `"yes"` and `"null"` stay strings in every YAML reader. |
| XML | `<records>` with one element per record. For XML input, strings become attributes and lists become repeated child elements, so XML → XML round-trips. For JSON input, values become child elements. Keys that aren't valid XML names are fixed (`1 bad` → `_1_bad`). |
| CSV / TSV | One row per record. Nested keys become dotted columns (`price.cur`). `#text` adds nothing to the column name, so `name: [{"#text": "Bolt"}]` is the column `name`. A column with several values (a list) holds them joined with `; `. The columns are every key seen in any record, in order of first appearance. |

Converting JSON to JSON is also a fast formatter: pretty-print a one-line 100 MB file, or minify an indented one. Numbers keep their exact text, and key order and duplicate keys are preserved.

### Clean-up and shape options

They are all off by default, which gives the plain mapping below. They apply to the converted file, the JSON panel, the tree and search.

- **Trim** removes spaces at the start and end of every value.
- **Collapse spaces** turns every run of spaces, tabs and newlines into one space.
- **Remove empty** drops empty strings, nulls, `{}` and `[]` everywhere, bottom-up. A record that becomes empty stays `{}`, so the record count never changes.
- **Unwrap 1-item lists** turns `[x]` into `x`.
- **Text as value** turns `{"#text": "Bolt"}` into `"Bolt"`.
- **Attr. prefix** (XML) puts a prefix in front of attribute names, for example `@id`.
- **Drop fields** removes keys by name wherever they occur. Use the names as they appear in the output, so `@id` when you use a prefix.
- **Simple** sets the last three the way xmltodict does: `@` attributes, plain text values, no one-item lists.

## Command line

```sh
tome parts.xml parts.json                   # JSON array, one compact record per line
tome parts.xml parts.json --pretty          # indented like json.dumps(..., indent=2)
tome parts.xml parts.ndjson                 # one object per line (format from the extension)
tome parts.xml parts.csv                    # also .tsv .yaml .yml .xml .jsonl
tome parts.xml parts.json --tag item        # another record element (default: part)
tome data.json data.csv --records data.items
tome big.json pretty.json --pretty          # reformat JSON
tome parts.xml clean.json --simple --trim --remove-empty --drop internal_id
tome --help                                 # every option
```

The output format follows the output file's extension. `.json` gives compact JSON unless you pass `--pretty`, and `--format NAME` overrides the extension. The exit code is 0 on success and 1 on an error (the message includes line and column).

## Mapping (XML to JSON)

This is the plain mapping. With every option off, the compact output is byte-for-byte identical to the original Python version.


- Attributes become string keys and are never converted to numbers, so `"00123"` stays `"00123"`.
- Child elements always become arrays, even when there is only one.
- Non-whitespace text before the first child is stored under `"#text"`, trimmed. Text after a child (mixed content) is dropped.
- Namespace prefixes are removed (`p:part` → `part`, `xml:lang` → `lang`), and `xmlns` declarations are dropped.
- An element with an attribute and a child of the same name is an error.
- CDATA counts as text. Comments and processing instructions are skipped. Line endings become `\n`, as in any XML parser.
- Input may be UTF-8 (with or without a BOM), UTF-16, ISO-8859-1 or any other declared encoding. Files that aren't UTF-8 are converted to UTF-8 in memory first.

Malformed XML is rejected the same way Python's expat rejects it: unclosed or mismatched tags, junk after the root element, bad entities or characters, duplicate attributes, and so on. One known difference: an undeclared namespace prefix is accepted.

## How it is fast

1. The file is memory-mapped. UTF-8 and the XML character rules are checked on all cores at once.
2. **Parallel scan.** The file is cut into chunks and every core finds tag boundaries in its own chunk, using SIMD-accelerated `memchr`. Each chunk starts at a guessed `<`. A quick sequential pass then checks the guesses, joins the nesting across chunks and rescans any chunk whose guess fell inside a comment or CDATA.
3. **Parallel conversion.** Records are parsed straight from the mapped bytes, copying strings only when entities have to be expanded. They are encoded to JSON in batches on all cores. A writer thread writes the batches in order to `<output>.tmp`, which is renamed when everything has succeeded.

For pretty output most of the remaining time is writing the roughly 1 GB file. The build uses `target-cpu=native` (`.cargo/config.toml`), so the binary is tuned for this CPU. Remove that line to build a binary that runs on other x86-64 machines.

## Tests

```sh
cargo build --release
python3 tests/compare_with_python.py --random 1000   # Rust vs the Python reference, byte for byte
cargo test --release                                 # window logic, run headless
TOME_BIG=big.xml cargo test --release -- --ignored --nocapture   # timings on a big file
```

`tests/compare_with_python.py` runs about 50 hand-written edge cases, including malformed files, plus random documents. Each one goes through the Python reference in `python/` and through the Rust binary in all three JSON formats. It also checks that the parallel scanner agrees with the sequential one at chunk sizes down to 1 byte.

For each case that converts, it also checks:

- The generic writers give the same bytes as the fast path.
- Feeding the JSON and NDJSON output back in as input gives exactly the same output again.
- The YAML loads (with PyYAML) as the same records.
- The XML output converts back to the same JSON.
- CSV and TSV match a Python model of the flattening.
- Four combinations of clean-up options match a Python model of them.

`python/` holds the original Python/GTK version. It is kept as the reference implementation for these tests.
