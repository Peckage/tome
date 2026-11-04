//! File handling and the parallel conversion pipeline.
//!
//! ```text
//!   scanner thread ──batches of record ranges──▶ N worker threads
//!                                                   │ parse + JSON encode
//!   main thread ◀──── encoded chunks (any order) ───┘
//!     reorders by sequence number and writes to <output>.tmp,
//!     renamed to <output> only when everything succeeded.
//! ```
//!
//! Both channels are bounded, so memory stays flat however large the input.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rayon::prelude::*;

use crate::json;
use crate::jsonin;
use crate::node::{self, Node, Options, XmlScalars, XmlStyle};
use crate::xml::{self, Entities, XResult, XmlError};

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

enum Data {
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}

/// What kind of records a file holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Xml,
    /// A JSON document, NDJSON, or concatenated JSON values.
    Json,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Xml => "XML",
            Kind::Json => "JSON",
        }
    }

    /// What selects the records: a tag name, or a key path.
    pub fn default_selector(self) -> &'static str {
        match self {
            Kind::Xml => "part",
            Kind::Json => "",
        }
    }
}

/// Guess the kind from the first non-blank character: `<` is XML, `{` or
/// `[` is JSON. `None` for anything else (plain text).
pub fn sniff_kind(path: &Path) -> Option<Kind> {
    use std::io::Read;
    let mut head = [0u8; 512];
    let n = File::open(path).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    let head = &head[..n];
    if head.starts_with(b"\xFF\xFE<") || head.starts_with(b"\xFE\xFF\0<") || head.starts_with(b"<\0") || head.starts_with(b"\0<") {
        return Some(Kind::Xml);
    }
    let head = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    match head.iter().find(|b| !b.is_ascii_whitespace()) {
        Some(b'<') => Some(Kind::Xml),
        Some(b'{' | b'[') => Some(Kind::Json),
        _ => None,
    }
}

/// An input file, memory-mapped (or transcoded to UTF-8 in memory when it is
/// not UTF-8), validated, with its DTD entities read.
pub struct Doc {
    pub kind: Kind,
    data: Data,
    pub path: PathBuf,
    /// Size of the file on disk.
    pub file_size: u64,
    /// Where the document starts in `bytes()` (after any BOM).
    pub origin: usize,
    pub entities: Entities,
    /// Set when the file was transcoded, e.g. "ISO-8859-1".
    pub transcoded_from: Option<String>,
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Xml { message: String, line: usize, column: usize },
    NoRecords(String),
    Cancelled,
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Xml { message, line, column } => write!(f, "{message}: line {line}, column {column}"),
            Error::NoRecords(tag) => write!(f, "no <{tag}> elements found"),
            Error::Cancelled => write!(f, "cancelled"),
            Error::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

impl Doc {
    /// Open as the kind the content suggests (XML when unsure).
    pub fn open(path: &Path) -> Result<Doc> {
        Doc::open_as(path, sniff_kind(path).unwrap_or(Kind::Xml))
    }

    pub fn open_as(path: &Path, kind: Kind) -> Result<Doc> {
        let file = File::open(path)?;
        let file_size = file.metadata()?.len();
        let data = if file_size == 0 {
            Data::Owned(Vec::new())
        } else {
            // SAFETY: the file is only read. If another program truncates it
            // while we run, the process may be killed (SIGBUS); that is the
            // documented trade-off of mapping instead of copying 500 MB.
            Data::Mapped(unsafe { memmap2::MmapOptions::new().map(&file)? })
        };
        let mut doc = Doc {
            kind,
            data,
            path: path.to_path_buf(),
            file_size,
            origin: 0,
            entities: Entities::new(),
            transcoded_from: None,
        };
        doc.prepare()?;
        Ok(doc)
    }

    pub fn bytes(&self) -> &[u8] {
        match &self.data {
            Data::Mapped(m) => m,
            Data::Owned(v) => v,
        }
    }

    fn prepare(&mut self) -> Result<()> {
        if self.kind == Kind::Json {
            if self.bytes().starts_with(b"\xEF\xBB\xBF") {
                self.origin = 3;
            }
            if let Err(offset) = validate_chars(self.bytes(), false) {
                return Err(self.xml_error(XmlError::new(offset, "invalid UTF-8")));
            }
            return Ok(());
        }
        if let Some((label, bom)) = detect_encoding(self.bytes()) {
            let decoded = transcode(&self.bytes()[bom..], &label)?;
            self.data = Data::Owned(decoded.into_bytes());
            self.transcoded_from = Some(label);
            self.origin = 0;
        } else if self.bytes().starts_with(b"\xEF\xBB\xBF") {
            self.origin = 3;
        }
        if let Err(offset) = validate_chars(self.bytes(), true) {
            return Err(self.xml_error(XmlError::new(offset, "not well-formed (invalid token)")));
        }
        self.entities = xml::read_prolog(self.bytes(), self.origin).map_err(|e| self.xml_error(e))?;
        Ok(())
    }

    pub fn xml_error(&self, e: XmlError) -> Error {
        let (line, column) = line_column(self.bytes(), e.offset);
        Error::Xml { message: e.message, line, column }
    }

    /// The XML element that probably holds the records. For JSON the
    /// records are found when indexing with an empty selector.
    pub fn guess_record_tag(&self) -> Option<String> {
        match self.kind {
            Kind::Xml => xml::guess_record_tag(self.bytes(), self.origin, 20_000),
            Kind::Json => None,
        }
    }
}

/// Returns `(encoding label, BOM length)` when the input is not UTF-8.
fn detect_encoding(b: &[u8]) -> Option<(String, usize)> {
    if b.starts_with(b"\xFF\xFE") {
        return Some(("UTF-16LE".into(), 2));
    }
    if b.starts_with(b"\xFE\xFF") {
        return Some(("UTF-16BE".into(), 2));
    }
    if b.starts_with(b"<\0?\0") {
        return Some(("UTF-16LE".into(), 0));
    }
    if b.starts_with(b"\0<\0?") {
        return Some(("UTF-16BE".into(), 0));
    }
    if b.starts_with(b"\xEF\xBB\xBF") || !b.starts_with(b"<?xml") {
        return None;
    }
    let decl_end = memchr::memmem::find(&b[..b.len().min(512)], b"?>")?;
    let decl = std::str::from_utf8(&b[..decl_end]).ok()?;
    let at = decl.find("encoding")?;
    let rest = decl[at + 8..].trim_start().strip_prefix('=')?.trim_start();
    let quote = rest.chars().next()?;
    let value = rest[1..].split(quote).next()?;
    match value.to_ascii_lowercase().as_str() {
        "utf-8" | "utf8" | "us-ascii" | "ascii" => None,
        _ => Some((value.to_string(), 0)),
    }
}

fn transcode(bytes: &[u8], label: &str) -> Result<String> {
    let lower = label.to_ascii_lowercase();
    // encoding_rs treats ISO-8859-1 as windows-1252 (the web's rule); XML
    // parsers, Python included, map every byte to the same code point.
    if matches!(lower.as_str(), "iso-8859-1" | "iso8859-1" | "latin1" | "latin-1" | "l1") {
        return Ok(bytes.iter().map(|&b| b as char).collect());
    }
    let encoding = encoding_rs::Encoding::for_label(lower.as_bytes())
        .ok_or_else(|| Error::Other(format!("unknown encoding: {label}")))?;
    let (text, had_errors) = encoding.decode_without_bom_handling(bytes);
    if had_errors {
        return Err(Error::Other(format!("file is not valid {label}")));
    }
    Ok(text.into_owned())
}

/// Check UTF-8 (and with `xml`, the XML 1.0 character set) in parallel.
/// Returns the offset of the first bad byte.
fn validate_chars(b: &[u8], xml: bool) -> std::result::Result<(), usize> {
    const CHUNK: usize = 4 << 20;
    let mut bounds = vec![0];
    let mut at = CHUNK;
    while at < b.len() {
        // Do not split inside a multi-byte character.
        while at < b.len() && (b[at] & 0xC0) == 0x80 {
            at += 1;
        }
        bounds.push(at);
        at += CHUNK;
    }
    bounds.push(b.len());
    let first_bad = bounds
        .par_windows(2)
        .filter_map(|w| {
            let (start, end) = (w[0], w[1]);
            let chunk = &b[start..end];
            let mut bad = match std::str::from_utf8(chunk) {
                Ok(_) => None,
                Err(e) => Some(e.valid_up_to()),
            };
            if !xml {
                return bad.map(|i| start + i);
            }
            let limit = bad.unwrap_or(chunk.len());
            if let Some(i) = chunk[..limit].iter().position(|&c| c < 0x20 && !matches!(c, b'\t' | b'\n' | b'\r')) {
                bad = Some(i);
            }
            let limit = bad.unwrap_or(chunk.len());
            for needle in [&b"\xEF\xBF\xBE"[..], &b"\xEF\xBF\xBF"[..]] {
                if let Some(i) = memchr::memmem::find(&chunk[..limit], needle) {
                    bad = Some(bad.map_or(i, |x: usize| x.min(i)));
                }
            }
            bad.map(|i| start + i)
        })
        .min();
    match first_bad {
        Some(offset) => Err(offset),
        None => Ok(()),
    }
}

/// 1-based line, 0-based column in characters, like expat reports.
pub fn line_column(b: &[u8], offset: usize) -> (usize, usize) {
    let offset = offset.min(b.len());
    let before = &b[..offset];
    let line = memchr::memchr_iter(b'\n', before).count() + 1;
    let line_start = memchr::memrchr(b'\n', before).map_or(0, |i| i + 1);
    let column = String::from_utf8_lossy(&before[line_start..]).chars().count();
    (line, column)
}

/// Count lines (`\n`, plus a final unterminated line) using all cores.
pub fn count_lines(path: &Path) -> Result<u64> {
    let file = File::open(path)?;
    if file.metadata()?.len() == 0 {
        return Ok(0);
    }
    let map = unsafe { memmap2::Mmap::map(&file)? };
    let newlines: usize = map.par_chunks(1 << 20).map(|c| memchr::memchr_iter(b'\n', c).count()).sum();
    let unterminated = (map.last() != Some(&b'\n')) as usize;
    Ok((newlines + unterminated) as u64)
}

// ---------------------------------------------------------------------------
// Progress and formats
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Progress {
    /// Input bytes the scanner has passed.
    pub bytes: AtomicU64,
    /// Records written (convert) or examined (search).
    pub records: AtomicU64,
    pub cancel: AtomicBool,
}

impl Progress {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// One JSON array, one compact record per line.
    Compact,
    /// One JSON array, indented like `json.dumps(..., indent=2)`.
    Pretty,
    /// One compact JSON object per line.
    Ndjson,
    /// A YAML sequence, one item per record. Strings are always quoted.
    Yaml,
    /// `<records>` with one element per record.
    Xml,
    /// One row per record, nested keys flattened into columns.
    Csv,
    Tsv,
}

impl Format {
    pub const ALL: [Format; 7] = [Format::Pretty, Format::Compact, Format::Ndjson, Format::Yaml, Format::Xml, Format::Csv, Format::Tsv];

    pub fn extension(self) -> &'static str {
        match self {
            Format::Compact | Format::Pretty => "json",
            Format::Ndjson => "ndjson",
            Format::Yaml => "yaml",
            Format::Xml => "xml",
            Format::Csv => "csv",
            Format::Tsv => "tsv",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Format::Pretty => "Pretty JSON",
            Format::Compact => "Compact JSON",
            Format::Ndjson => "NDJSON",
            Format::Yaml => "YAML",
            Format::Xml => "XML",
            Format::Csv => "CSV",
            Format::Tsv => "TSV",
        }
    }

    /// The format an output file name asks for. `.json` is not listed: it
    /// could be compact or pretty, so the caller decides.
    pub fn from_extension(ext: &str) -> Option<Format> {
        match ext.to_ascii_lowercase().as_str() {
            "ndjson" | "jsonl" => Some(Format::Ndjson),
            "yaml" | "yml" => Some(Format::Yaml),
            "xml" => Some(Format::Xml),
            "csv" => Some(Format::Csv),
            "tsv" | "tab" => Some(Format::Tsv),
            _ => None,
        }
    }

    pub fn is_json(self) -> bool {
        matches!(self, Format::Compact | Format::Pretty | Format::Ndjson)
    }

    pub fn is_table(self) -> bool {
        matches!(self, Format::Csv | Format::Tsv)
    }
}

/// The fast path: XML mapping straight to JSON, no options.
fn encode_record(out: &mut Vec<u8>, obj: &xml::Obj, format: Format, first: bool) {
    match format {
        Format::Compact => {
            out.extend_from_slice(if first { b"\n" } else { b",\n" });
            json::write_compact(out, obj);
        }
        Format::Pretty => {
            out.extend_from_slice(if first { b"\n  " } else { b",\n  " });
            json::write_pretty(out, obj, 2);
        }
        Format::Ndjson => {
            json::write_compact(out, obj);
            out.push(b'\n');
        }
        _ => unreachable!("only JSON formats take the fast path"),
    }
}

/// Turns records into output bytes for one format. Saving and the live
/// preview both use it, so the preview shows exactly what is written.
pub struct Encoder {
    pub format: Format,
    options: Options,
    record_name: String,
    scalars: XmlScalars,
    /// CSV/TSV columns (empty for other formats).
    pub columns: Vec<String>,
    column_index: HashMap<String, usize>,
    width: usize,
    /// Use the original XML-to-JSON writers.
    fast: bool,
}

impl Encoder {
    /// `columns` matters only for CSV/TSV; see [`collect_columns`].
    pub fn new(doc: &Doc, selector: &str, format: Format, options: &Options, columns: Vec<String>) -> Encoder {
        let record_name = match doc.kind {
            Kind::Xml => xml::local_name(selector).to_string(),
            Kind::Json => "record".to_string(),
        };
        Encoder {
            format,
            options: options.clone(),
            record_name,
            scalars: if doc.kind == Kind::Xml { XmlScalars::Attributes } else { XmlScalars::Elements },
            column_index: columns.iter().enumerate().map(|(i, c)| (c.clone(), i)).collect(),
            width: columns.len(),
            columns,
            fast: fast_path(doc, options) && format.is_json(),
        }
    }

    fn delimiter(&self) -> u8 {
        if self.format == Format::Tsv { b'\t' } else { b',' }
    }

    /// What comes before the first record.
    pub fn header(&self) -> Vec<u8> {
        let mut header = Vec::new();
        match self.format {
            Format::Compact | Format::Pretty => header.push(b'['),
            Format::Xml => header.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<records>\n"),
            Format::Csv | Format::Tsv => {
                let delimiter = self.delimiter();
                for (i, c) in self.columns.iter().enumerate() {
                    if i > 0 {
                        header.push(delimiter);
                    }
                    node::write_csv_field(&mut header, c, delimiter);
                }
                header.push(b'\n');
            }
            Format::Ndjson | Format::Yaml => {}
        }
        header
    }

    /// What comes after the last record.
    pub fn footer(&self) -> &'static [u8] {
        match self.format {
            Format::Compact | Format::Pretty => b"\n]\n",
            Format::Xml => b"</records>\n",
            _ => b"",
        }
    }

    /// Append record `range` as it appears in the output file.
    fn encode_range(&self, doc: &Doc, range: (usize, usize), out: &mut Vec<u8>, first: bool) -> XResult<()> {
        if self.fast {
            let obj = xml::parse_record(doc.bytes(), range, &doc.entities)?;
            encode_record(out, &obj, self.format, first);
        } else {
            let node = node_of(doc, range, &self.options)?;
            self.encode(out, &node, first);
        }
        Ok(())
    }

    /// Record `index` of `total` as lines of the output file: the bytes it
    /// adds, with the separators moved so that joining every block (header,
    /// records, footer) with newlines gives the file exactly.
    pub fn preview_block(&self, doc: &Doc, range: (usize, usize), index: usize, total: usize) -> Result<String> {
        let mut out = Vec::new();
        self.encode_range(doc, range, &mut out, index == 0).map_err(|e| doc.xml_error(e))?;
        let mut text = String::from_utf8(out).expect("output is UTF-8");
        match self.format {
            Format::Compact | Format::Pretty => {
                let body = text.strip_prefix(",\n").or_else(|| text.strip_prefix('\n')).unwrap_or(&text);
                let mut body = body.to_string();
                if index + 1 < total {
                    body.push(',');
                }
                text = body;
            }
            _ => {
                if text.ends_with('\n') {
                    text.pop();
                }
            }
        }
        Ok(text)
    }

    /// Header and footer as preview lines (`None` when the format has none).
    pub fn preview_header(&self) -> Option<String> {
        let h = String::from_utf8(self.header()).expect("UTF-8");
        let h = h.strip_suffix('\n').unwrap_or(&h).to_string();
        if h.is_empty() && self.width == 0 && !self.format.is_table() { None } else { Some(h) }
    }

    pub fn preview_footer(&self) -> Option<String> {
        let f = std::str::from_utf8(self.footer()).expect("UTF-8");
        let f = f.trim_start_matches('\n').trim_end_matches('\n');
        if f.is_empty() { None } else { Some(f.to_string()) }
    }

    /// One record on its own, for the record panel: pretty JSON for the
    /// JSON formats, the header plus the row for CSV/TSV.
    pub fn render_one(&self, doc: &Doc, range: (usize, usize)) -> Result<String> {
        let text = match self.format {
            Format::Pretty | Format::Compact | Format::Ndjson => return record_json(doc, range, self.format == Format::Pretty, &self.options),
            Format::Csv | Format::Tsv => {
                let mut header = String::from_utf8(self.header()).expect("UTF-8");
                header.push_str(&self.preview_block(doc, range, 0, 1)?);
                header
            }
            Format::Yaml | Format::Xml => self.preview_block(doc, range, 0, 1)?,
        };
        Ok(text)
    }

    fn encode(&self, out: &mut Vec<u8>, node: &Node, first: bool) {
        match self.format {
            Format::Compact => {
                out.extend_from_slice(if first { b"\n" } else { b",\n" });
                node::write_json_compact(out, node);
            }
            Format::Pretty => {
                out.extend_from_slice(if first { b"\n  " } else { b",\n  " });
                node::write_json_pretty(out, node, 2);
            }
            Format::Ndjson => {
                node::write_json_compact(out, node);
                out.push(b'\n');
            }
            Format::Yaml => node::write_yaml_record(out, node),
            Format::Xml => {
                out.extend_from_slice(b"  ");
                let style = XmlStyle { scalars: self.scalars, attr_prefix: &self.options.attr_prefix };
                node::write_xml_element(out, &self.record_name, node, 2, &style);
                out.push(b'\n');
            }
            Format::Csv | Format::Tsv => {
                let delimiter = self.delimiter();
                let mut cells = Vec::new();
                node::flatten(node, &mut cells);
                let mut row: Vec<&str> = vec![""; self.width];
                for (column, value) in &cells {
                    if let Some(&i) = self.column_index.get(column.as_str()) {
                        row[i] = value;
                    }
                }
                if self.width == 1 && row[0].is_empty() {
                    // An empty line would be read as no row at all.
                    out.extend_from_slice(b"\"\"");
                }
                for (i, cell) in row.iter().enumerate() {
                    if i > 0 {
                        out.push(delimiter);
                    }
                    node::write_csv_field(out, cell, delimiter);
                }
                out.push(b'\n');
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Conversion
// ---------------------------------------------------------------------------

pub fn worker_count() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get()).max(1)
}

/// Chunk size for the parallel scanner: enough chunks to keep every core
/// busy, large enough that per-chunk overhead does not matter.
fn scan_chunk_size(len: usize) -> usize {
    (len / (worker_count() * 8)).clamp(256 << 10, 64 << 20)
}

/// Find every record. Ranges before the first structural error are returned
/// along with the error, so callers can report an earlier record error first.
pub fn scan(doc: &Doc, tag: &str) -> crate::pscan::ScanResult {
    let bytes = doc.bytes();
    crate::pscan::scan(bytes, doc.origin, tag, &doc.entities, scan_chunk_size(bytes.len()))
}

/// Convert `doc` to `output`. Returns the number of records written.
/// `selector` is the record tag (XML) or key path (JSON, "" = automatic).
pub fn convert(doc: &Doc, selector: &str, output: &Path, format: Format, options: &Options, progress: &Progress) -> Result<u64> {
    if same_file(&doc.path, output) {
        return Err(Error::Other("output must be a different file from the input".into()));
    }
    let mut temporary = output.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);

    match convert_to(doc, selector, &temporary, format, options, progress) {
        Ok(count) => {
            std::fs::rename(&temporary, output)?;
            Ok(count)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&temporary);
            Err(e)
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

const BATCH: usize = 2048;

/// Find the records: their byte ranges, plus (XML only) a structural error
/// found after the last good record, reported once those are written.
/// Also returns the selector that was used (the automatic JSON path).
fn records(doc: &Doc, selector: &str) -> Result<(Vec<(usize, usize)>, Option<XmlError>, String)> {
    match doc.kind {
        Kind::Xml => {
            let scanned = scan(doc, selector);
            Ok((scanned.ranges, scanned.error, selector.to_string()))
        }
        Kind::Json => {
            // An .ndjson/.jsonl file is one record per line even when it
            // holds a single object that would look like a whole document.
            let lines = doc.path.extension().is_some_and(|e| matches!(e.to_string_lossy().to_ascii_lowercase().as_str(), "ndjson" | "jsonl"));
            let selector = if selector.is_empty() && lines { "lines" } else { selector };
            let located = jsonin::locate(doc.bytes(), doc.origin, selector).map_err(|e| doc.xml_error(e))?;
            Ok((located.ranges, None, located.path))
        }
    }
}

/// Parse one record into the generic model with the options applied.
fn node_of<'d>(doc: &'d Doc, range: (usize, usize), options: &Options) -> XResult<Node<'d>> {
    let node = match doc.kind {
        Kind::Xml => node::from_obj(xml::parse_record(doc.bytes(), range, &doc.entities)?, &options.attr_prefix),
        Kind::Json => jsonin::parse(doc.bytes(), range)?,
    };
    Ok(options.apply(node))
}

/// Does this record go through the original XML-to-JSON writers?
fn fast_path(doc: &Doc, options: &Options) -> bool {
    doc.kind == Kind::Xml && options.is_plain() && !FORCE_GENERIC.load(Ordering::Relaxed)
}

/// Test hook: send every record through the generic model, to prove it
/// writes the same bytes as the fast path.
pub static FORCE_GENERIC: AtomicBool = AtomicBool::new(false);

/// CSV columns: every flattened key, in order of first appearance.
pub fn collect_columns(doc: &Doc, ranges: &[(usize, usize)], options: &Options, progress: &Progress) -> Result<Vec<String>> {
    let per_batch: Vec<XResult<Vec<String>>> = ranges
        .par_chunks(BATCH)
        .map(|batch| {
            let mut seen = HashSet::new();
            let mut order = Vec::new();
            let mut cells = Vec::new();
            if progress.cancelled() {
                return Ok(order);
            }
            for &range in batch {
                cells.clear();
                node::flatten(&node_of(doc, range, options)?, &mut cells);
                for (column, _) in cells.drain(..) {
                    if seen.insert(column.clone()) {
                        order.push(column);
                    }
                }
            }
            Ok(order)
        })
        .collect();
    let mut seen = HashSet::new();
    let mut columns = Vec::new();
    for batch in per_batch {
        for column in batch.map_err(|e| doc.xml_error(e))? {
            if seen.insert(column.clone()) {
                columns.push(column);
            }
        }
    }
    if progress.cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(columns)
}

fn convert_to(doc: &Doc, selector: &str, temporary: &Path, format: Format, options: &Options, progress: &Progress) -> Result<u64> {
    let file = File::create(temporary)?;
    let (ranges, late_error, _) = records(doc, selector)?;
    let ranges = &ranges;
    if progress.cancelled() {
        return Err(Error::Cancelled);
    }
    if ranges.is_empty() {
        return Err(match late_error {
            Some(e) => doc.xml_error(e),
            None => Error::NoRecords(selector.to_string()),
        });
    }

    let columns = if format.is_table() { collect_columns(doc, ranges, options, progress)? } else { Vec::new() };
    let encoder = Encoder::new(doc, selector, format, options, columns);
    let header = encoder.header();
    let footer = encoder.footer();

    let bytes = doc.bytes();
    let window = BATCH * worker_count() * 2;
    let (tx, rx) = crossbeam_channel::bounded::<Vec<Vec<u8>>>(2);

    std::thread::scope(|scope| -> Result<()> {
        // Writer thread: writing overlaps with encoding the next window.
        let writer = scope.spawn(move || -> io::Result<()> {
            let mut file = io::BufWriter::with_capacity(1 << 20, file);
            file.write_all(&header)?;
            for chunks in rx {
                for chunk in chunks {
                    file.write_all(&chunk)?;
                }
            }
            file.write_all(footer)?;
            file.flush()
        });

        let mut failure: Option<Error> = None;
        for (w, records) in ranges.chunks(window).enumerate() {
            if progress.cancelled() {
                failure = Some(Error::Cancelled);
                break;
            }
            let base = w * window;
            let encoded: Vec<std::result::Result<Vec<u8>, XmlError>> = records
                .par_chunks(BATCH)
                .enumerate()
                .map(|(b, batch)| {
                    let mut out = Vec::with_capacity(batch.iter().map(|r| r.1 - r.0).sum::<usize>() * 2);
                    for (i, &range) in batch.iter().enumerate() {
                        encoder.encode_range(doc, range, &mut out, base + b * BATCH + i == 0)?;
                    }
                    Ok(out)
                })
                .collect();
            let mut chunks = Vec::with_capacity(encoded.len());
            for chunk in encoded {
                match chunk {
                    Ok(c) => chunks.push(c),
                    Err(e) => {
                        failure = Some(doc.xml_error(e));
                        break;
                    }
                }
            }
            if failure.is_some() || tx.send(chunks).is_err() {
                break;
            }
            let done = base + records.len();
            progress.records.store(done as u64, Ordering::Relaxed);
            progress.bytes.store(ranges[done - 1].1 as u64, Ordering::Relaxed);
        }
        drop(tx);
        let written = writer.join().expect("writer thread panicked");
        if let Some(e) = failure {
            return Err(e);
        }
        written?;
        Ok(())
    })?;

    if let Some(e) = late_error {
        return Err(doc.xml_error(e));
    }
    progress.bytes.store(bytes.len() as u64, Ordering::Relaxed);
    Ok(ranges.len() as u64)
}

/// Test hook (`--via-preview`): write the file by joining the preview
/// blocks, to prove the preview shows exactly what saving writes.
pub fn write_via_preview(doc: &Doc, selector: &str, output: &Path, format: Format, options: &Options) -> Result<u64> {
    let (ranges, error, _) = records(doc, selector)?;
    if let Some(e) = error {
        return Err(doc.xml_error(e));
    }
    if ranges.is_empty() {
        return Err(Error::NoRecords(selector.to_string()));
    }
    let columns = if format.is_table() { collect_columns(doc, &ranges, options, &Progress::default())? } else { Vec::new() };
    let encoder = Encoder::new(doc, selector, format, options, columns);
    let mut blocks = Vec::new();
    blocks.extend(encoder.preview_header());
    for (i, &range) in ranges.iter().enumerate() {
        blocks.push(encoder.preview_block(doc, range, i, ranges.len())?);
    }
    blocks.extend(encoder.preview_footer());
    let mut text = blocks.join("\n");
    text.push('\n');
    std::fs::write(output, text)?;
    Ok(ranges.len() as u64)
}

// ---------------------------------------------------------------------------
// Browsing and search
// ---------------------------------------------------------------------------

/// Byte ranges of every record, for random access.
pub fn index_records(doc: &Doc, selector: &str) -> Result<Vec<(usize, usize)>> {
    index(doc, selector).map(|(r, _)| r)
}

/// Byte ranges of every record and the selector that found them (for JSON
/// with an empty selector, the path that was picked automatically).
pub fn index(doc: &Doc, selector: &str) -> Result<(Vec<(usize, usize)>, String)> {
    let (ranges, error, used) = records(doc, selector)?;
    if let Some(e) = error {
        return Err(doc.xml_error(e));
    }
    Ok((ranges, used))
}

/// One record in the generic model, with the options applied.
pub fn parse_node<'d>(doc: &'d Doc, range: (usize, usize), options: &Options) -> Result<Node<'d>> {
    node_of(doc, range, options).map_err(|e| doc.xml_error(e))
}

/// The JSON that converting this record writes.
pub fn record_json(doc: &Doc, range: (usize, usize), pretty: bool, options: &Options) -> Result<String> {
    let mut out = Vec::new();
    if fast_path(doc, options) {
        let obj = xml::parse_record(doc.bytes(), range, &doc.entities).map_err(|e| doc.xml_error(e))?;
        if pretty { json::write_pretty(&mut out, &obj, 0) } else { json::write_compact(&mut out, &obj) }
    } else {
        let node = parse_node(doc, range, options)?;
        if pretty { node::write_json_pretty(&mut out, &node, 0) } else { node::write_json_compact(&mut out, &node) }
    }
    Ok(String::from_utf8(out).expect("JSON output is UTF-8"))
}

/// Compact JSON of one record into `buf` (cleared first).
fn compact_into(doc: &Doc, range: (usize, usize), options: &Options, fast: bool, buf: &mut Vec<u8>) -> XResult<()> {
    buf.clear();
    if fast {
        json::write_compact(buf, &xml::parse_record(doc.bytes(), range, &doc.entities)?);
    } else {
        node::write_json_compact(buf, &node_of(doc, range, options)?);
    }
    Ok(())
}

/// Indices (into `ranges`) of records whose JSON contains `needle`.
/// Case-insensitive unless the needle has an uppercase letter.
pub fn search(doc: &Doc, ranges: &[(usize, usize)], needle: &str, options: &Options, progress: &Progress) -> Result<Vec<u32>> {
    let fast = fast_path(doc, options);
    let case_sensitive = needle.chars().any(char::is_uppercase);
    // Look for the needle as it appears inside a JSON string, so quotes and
    // backslashes in the search text match their escaped form.
    let mut encoded = Vec::new();
    json::write_str(&mut encoded, needle);
    let mut needle_text = String::from_utf8(encoded[1..encoded.len() - 1].to_vec()).unwrap();
    if !case_sensitive {
        needle_text = needle_text.to_lowercase();
    }
    let finder = memchr::memmem::Finder::new(needle_text.as_bytes()).into_owned();
    let ascii_needle = needle_text.is_ascii();
    let error: Mutex<Option<XmlError>> = Mutex::new(None);

    let hits: Vec<u32> = ranges
        .par_chunks(2048)
        .enumerate()
        .flat_map_iter(|(chunk_index, chunk)| {
            let mut found = Vec::new();
            if progress.cancelled() {
                return found;
            }
            let mut buf = Vec::with_capacity(4096);
            for (i, &range) in chunk.iter().enumerate() {
                match compact_into(doc, range, options, fast, &mut buf) {
                    Ok(()) => {}
                    Err(e) => {
                        let mut slot = error.lock().unwrap();
                        if slot.as_ref().is_none_or(|old| e.offset < old.offset) {
                            *slot = Some(e);
                        }
                        progress.cancel.store(true, Ordering::Relaxed);
                        return found;
                    }
                };
                let hit = if case_sensitive {
                    finder.find(&buf).is_some()
                } else if ascii_needle {
                    buf.make_ascii_lowercase();
                    finder.find(&buf).is_some()
                } else {
                    let lowered = String::from_utf8_lossy(&buf).to_lowercase();
                    finder.find(lowered.as_bytes()).is_some()
                };
                if hit {
                    found.push((chunk_index * 2048 + i) as u32);
                }
            }
            progress.records.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            found
        })
        .collect();

    if let Some(e) = error.into_inner().unwrap() {
        return Err(doc.xml_error(e));
    }
    if progress.cancelled() {
        return Err(Error::Cancelled);
    }
    Ok(hits)
}
