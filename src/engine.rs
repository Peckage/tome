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

use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rayon::prelude::*;

use crate::json;
use crate::xml::{self, Entities, XmlError};

// ---------------------------------------------------------------------------
// Documents
// ---------------------------------------------------------------------------

enum Data {
    Mapped(memmap2::Mmap),
    Owned(Vec<u8>),
}

/// An input file, memory-mapped (or transcoded to UTF-8 in memory when it is
/// not UTF-8), validated, with its DTD entities read.
pub struct Doc {
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
    pub fn open(path: &Path) -> Result<Doc> {
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
        if let Some((label, bom)) = detect_encoding(self.bytes()) {
            let decoded = transcode(&self.bytes()[bom..], &label)?;
            self.data = Data::Owned(decoded.into_bytes());
            self.transcoded_from = Some(label);
            self.origin = 0;
        } else if self.bytes().starts_with(b"\xEF\xBB\xBF") {
            self.origin = 3;
        }
        if let Err(offset) = validate_chars(self.bytes()) {
            return Err(self.xml_error(XmlError::new(offset, "not well-formed (invalid token)")));
        }
        self.entities = xml::read_prolog(self.bytes(), self.origin).map_err(|e| self.xml_error(e))?;
        Ok(())
    }

    pub fn xml_error(&self, e: XmlError) -> Error {
        let (line, column) = line_column(self.bytes(), e.offset);
        Error::Xml { message: e.message, line, column }
    }

    pub fn guess_record_tag(&self) -> Option<String> {
        xml::guess_record_tag(self.bytes(), self.origin, 20_000)
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

/// Check UTF-8 and the XML 1.0 character set in parallel. Returns the offset
/// of the first bad byte.
fn validate_chars(b: &[u8]) -> std::result::Result<(), usize> {
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
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Ndjson => "ndjson",
            _ => "json",
        }
    }
}

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
pub fn convert(doc: &Doc, tag: &str, output: &Path, format: Format, progress: &Progress) -> Result<u64> {
    if same_file(&doc.path, output) {
        return Err(Error::Other("output must be a different file from the input".into()));
    }
    let mut temporary = output.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);

    match convert_to(doc, tag, &temporary, format, progress) {
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

fn convert_to(doc: &Doc, tag: &str, temporary: &Path, format: Format, progress: &Progress) -> Result<u64> {
    let file = File::create(temporary)?;
    let scanned = scan(doc, tag);
    let ranges = &scanned.ranges;
    if progress.cancelled() {
        return Err(Error::Cancelled);
    }
    if ranges.is_empty() {
        return Err(match scanned.error {
            Some(e) => doc.xml_error(e),
            None => Error::NoRecords(tag.to_string()),
        });
    }

    let bytes = doc.bytes();
    let window = BATCH * worker_count() * 2;
    let (tx, rx) = crossbeam_channel::bounded::<Vec<Vec<u8>>>(2);

    std::thread::scope(|scope| -> Result<()> {
        // Writer thread: writing overlaps with encoding the next window.
        let writer = scope.spawn(move || -> io::Result<()> {
            let mut file = file;
            if format != Format::Ndjson {
                file.write_all(b"[")?;
            }
            for chunks in rx {
                for chunk in chunks {
                    file.write_all(&chunk)?;
                }
            }
            if format != Format::Ndjson {
                file.write_all(b"\n]\n")?;
            }
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
                        let obj = xml::parse_record(bytes, range, &doc.entities)?;
                        encode_record(&mut out, &obj, format, base + b * BATCH + i == 0);
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

    if let Some(e) = scanned.error {
        return Err(doc.xml_error(e));
    }
    progress.bytes.store(bytes.len() as u64, Ordering::Relaxed);
    Ok(ranges.len() as u64)
}

// ---------------------------------------------------------------------------
// Browsing and search
// ---------------------------------------------------------------------------

/// Byte ranges of every record, for random access.
pub fn index_records(doc: &Doc, tag: &str) -> Result<Vec<(usize, usize)>> {
    let scanned = scan(doc, tag);
    match scanned.error {
        Some(e) => Err(doc.xml_error(e)),
        None => Ok(scanned.ranges),
    }
}

pub fn parse_record<'d>(doc: &'d Doc, range: (usize, usize)) -> Result<xml::Obj<'d>> {
    xml::parse_record(doc.bytes(), range, &doc.entities).map_err(|e| doc.xml_error(e))
}

pub fn record_json(doc: &Doc, range: (usize, usize), pretty: bool) -> Result<String> {
    let obj = parse_record(doc, range)?;
    let mut out = Vec::new();
    if pretty {
        json::write_pretty(&mut out, &obj, 0);
    } else {
        json::write_compact(&mut out, &obj);
    }
    Ok(String::from_utf8(out).expect("JSON output is UTF-8"))
}

/// Indices (into `ranges`) of records whose JSON contains `needle`.
/// Case-insensitive unless the needle has an uppercase letter.
pub fn search(doc: &Doc, ranges: &[(usize, usize)], needle: &str, progress: &Progress) -> Result<Vec<u32>> {
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
                let obj = match xml::parse_record(doc.bytes(), range, &doc.entities) {
                    Ok(o) => o,
                    Err(e) => {
                        let mut slot = error.lock().unwrap();
                        if slot.as_ref().is_none_or(|old| e.offset < old.offset) {
                            *slot = Some(e);
                        }
                        progress.cancel.store(true, Ordering::Relaxed);
                        return found;
                    }
                };
                buf.clear();
                json::write_compact(&mut buf, &obj);
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
