//! Raw text access for files of any size and kind.
//!
//! The file is memory-mapped and its line starts are found on all cores, so
//! opening a 500 MB log takes a fraction of a second and any line can be
//! shown immediately. Nothing here requires the file to be valid UTF-8 or
//! valid anything: bytes are decoded lossily only when a line is drawn.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::Ordering;

use rayon::prelude::*;

use crate::engine::{Error, Progress, Result};

enum Data {
    Mapped(memmap2::Mmap),
    Empty,
}

pub struct TextFile {
    data: Data,
    /// Byte offset where each line starts. Always has at least one entry for
    /// a non-empty file; a final `\n` does not start an extra line.
    starts: Vec<u64>,
    /// True when the first line break found is `\r\n`.
    pub crlf: bool,
    /// Rough guess from the first bytes: "UTF-8", "UTF-16LE", "binary"…
    pub encoding: &'static str,
    /// Length of the longest line, in bytes.
    pub longest_line: u64,
}

/// Chunk size used to split work across cores.
const CHUNK: usize = 4 << 20;

impl TextFile {
    pub fn open(path: &Path) -> Result<TextFile> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        let data = if len == 0 {
            Data::Empty
        } else {
            // SAFETY: read-only mapping; see engine::Doc::open for the
            // trade-off if another program truncates the file meanwhile.
            Data::Mapped(unsafe { memmap2::MmapOptions::new().map(&file)? })
        };
        let mut text = TextFile { data, starts: Vec::new(), crlf: false, encoding: "UTF-8", longest_line: 0 };
        text.index();
        Ok(text)
    }

    pub fn bytes(&self) -> &[u8] {
        match &self.data {
            Data::Mapped(m) => m,
            Data::Empty => &[],
        }
    }

    fn index(&mut self) {
        let bytes = self.bytes();
        if bytes.is_empty() {
            return;
        }
        let per_chunk: Vec<Vec<u64>> = bytes
            .par_chunks(CHUNK)
            .enumerate()
            .map(|(c, chunk)| {
                let base = (c * CHUNK) as u64;
                memchr::memchr_iter(b'\n', chunk).map(|i| base + i as u64 + 1).collect()
            })
            .collect();
        let total: usize = per_chunk.iter().map(Vec::len).sum();
        let mut starts = Vec::with_capacity(total + 1);
        starts.push(0u64);
        for chunk in per_chunk {
            starts.extend(chunk);
        }
        if starts.last() == Some(&(bytes.len() as u64)) {
            starts.pop(); // a final newline ends the last line, it does not start one
        }
        let longest = starts
            .par_windows(2)
            .map(|w| w[1] - w[0])
            .max()
            .unwrap_or(0)
            .max(bytes.len() as u64 - starts.last().copied().unwrap_or(0));
        let crlf = memchr::memchr(b'\n', bytes).is_some_and(|i| i > 0 && bytes[i - 1] == b'\r');
        let encoding = guess_encoding(bytes);
        self.starts = starts;
        self.crlf = crlf;
        self.encoding = encoding;
        self.longest_line = longest;
    }

    pub fn len(&self) -> u64 {
        self.bytes().len() as u64
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Byte range of line `i`, without its line break.
    pub fn line_range(&self, i: usize) -> (usize, usize) {
        let bytes = self.bytes();
        let start = self.starts[i] as usize;
        let mut end = self.starts.get(i + 1).map_or(bytes.len(), |&s| s as usize);
        if end > start && bytes[end - 1] == b'\n' {
            end -= 1;
        }
        if end > start && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        (start, end)
    }

    pub fn line(&self, i: usize) -> &[u8] {
        let (s, e) = self.line_range(i);
        &self.bytes()[s..e]
    }

    /// The line that contains byte `offset`.
    pub fn line_of(&self, offset: u64) -> usize {
        self.starts.partition_point(|&s| s <= offset).saturating_sub(1)
    }

    pub fn line_start(&self, i: usize) -> u64 {
        self.starts[i]
    }

    /// Lines `first..=last` joined with `\n`, at most `limit` bytes.
    /// Returns the text and whether it was cut short.
    pub fn copy_lines(&self, first: usize, last: usize, limit: usize) -> (String, bool) {
        let start = self.starts[first] as usize;
        let end = self.line_range(last).1;
        let cut = end - start > limit;
        let end = if cut { start + limit } else { end };
        let text = String::from_utf8_lossy(&self.bytes()[start..end]);
        let text = if self.crlf { text.replace("\r\n", "\n") } else { text.into_owned() };
        (text, cut)
    }
}

fn guess_encoding(b: &[u8]) -> &'static str {
    if b.starts_with(b"\xFF\xFE") || b.starts_with(b"<\0") {
        return "UTF-16LE";
    }
    if b.starts_with(b"\xFE\xFF") || b.starts_with(b"\0<") {
        return "UTF-16BE";
    }
    let head = &b[..b.len().min(64 << 10)];
    if memchr::memchr(0, head).is_some() {
        return "binary";
    }
    match std::str::from_utf8(head) {
        Ok(_) => "UTF-8",
        // A multi-byte character cut by the sample boundary is still UTF-8.
        Err(e) if e.error_len().is_none() => "UTF-8",
        Err(_) => "8-bit",
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

pub enum Pattern {
    Literal(memchr::memmem::Finder<'static>),
    Regex(regex::bytes::Regex),
}

impl Pattern {
    /// `regex`: treat `needle` as a regular expression. Matching ignores case
    /// unless the needle has an uppercase letter.
    pub fn new(needle: &str, regex: bool) -> Result<Pattern> {
        let ignore_case = !needle.chars().any(char::is_uppercase);
        if !regex && !ignore_case {
            return Ok(Pattern::Literal(memchr::memmem::Finder::new(needle.as_bytes()).into_owned()));
        }
        let source = if regex { needle.to_string() } else { regex::escape(needle) };
        regex::bytes::RegexBuilder::new(&source)
            .case_insensitive(ignore_case)
            .multi_line(true)
            .unicode(true)
            .build()
            .map(Pattern::Regex)
            .map_err(|e| Error::Other(format!("bad regular expression: {e}")))
    }

    /// Every match in `hay` as (start, end), not overlapping.
    pub fn for_each(&self, hay: &[u8], mut f: impl FnMut(usize, usize) -> bool) {
        match self {
            Pattern::Literal(finder) => {
                let n = finder.needle().len().max(1);
                let mut at = 0;
                while let Some(i) = finder.find(&hay[at..]) {
                    if !f(at + i, at + i + finder.needle().len()) {
                        return;
                    }
                    at += i + n;
                    if at > hay.len() {
                        return;
                    }
                }
            }
            Pattern::Regex(re) => {
                for m in re.find_iter(hay) {
                    if m.start() == m.end() {
                        continue; // empty matches are useless for navigation
                    }
                    if !f(m.start(), m.end()) {
                        return;
                    }
                }
            }
        }
    }
}

/// Matches as absolute byte (start, end) ranges in file order. Work is split
/// at line breaks, so a match never spans two chunks; regex matches may span
/// lines only inside one chunk. At most `limit` matches are kept.
pub fn find_all(text: &TextFile, pattern: &Pattern, limit: usize, progress: &Progress) -> Result<(Vec<(u64, u64)>, bool)> {
    let bytes = text.bytes();
    let mut bounds = vec![0usize];
    let mut at = CHUNK;
    while at < bytes.len() {
        match memchr::memchr(b'\n', &bytes[at..]) {
            Some(i) => {
                at += i + 1;
                bounds.push(at);
                at += CHUNK;
            }
            None => break,
        }
    }
    bounds.push(bytes.len());
    bounds.dedup();

    let per_chunk: Vec<Vec<(u64, u64)>> = bounds
        .par_windows(2)
        .map(|w| {
            let mut found = Vec::new();
            if progress.cancel.load(Ordering::Relaxed) {
                return found;
            }
            let (start, end) = (w[0], w[1]);
            pattern.for_each(&bytes[start..end], |s, e| {
                found.push(((start + s) as u64, (start + e) as u64));
                found.len() < limit
            });
            progress.bytes.fetch_add((end - start) as u64, Ordering::Relaxed);
            found
        })
        .collect();
    if progress.cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let mut all = Vec::new();
    let mut truncated = false;
    for chunk in per_chunk {
        let room = limit - all.len();
        if chunk.len() > room {
            all.extend_from_slice(&chunk[..room]);
            truncated = true;
            break;
        }
        all.extend(chunk);
    }
    Ok((all, truncated))
}

/// Text to draw for one line: lossy UTF-8, tabs expanded, at most
/// `max_chars` characters starting `skip` characters in. Only looks at a
/// bounded window of bytes, so a 500 MB single-line file is still instant.
pub fn display_window(line: &[u8], skip: usize, max_chars: usize) -> String {
    let window_end = line.len().min((skip + max_chars) * 4 + 4);
    let text = String::from_utf8_lossy(&line[..window_end]);
    let mut out = String::with_capacity(max_chars + 8);
    let (mut col, mut shown) = (0usize, 0usize);
    for c in text.chars() {
        let (glyph, width) = match c {
            '\t' => (' ', 4 - col % 4),
            c if (c as u32) < 0x20 || c == '\u{7f}' => ('·', 1),
            c => (c, 1),
        };
        for _ in 0..width {
            if col >= skip {
                if shown >= max_chars {
                    return out;
                }
                out.push(glyph);
                shown += 1;
            }
            col += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, content: &[u8]) -> TextFile {
        let dir = std::env::temp_dir().join(format!("tome-text-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        TextFile::open(&path).unwrap()
    }

    #[test]
    fn lines_and_endings() {
        let t = file("a.txt", b"one\r\ntwo\r\n\r\nlast");
        assert_eq!(t.line_count(), 4);
        assert!(t.crlf);
        assert_eq!(t.line(0), b"one");
        assert_eq!(t.line(2), b"");
        assert_eq!(t.line(3), b"last");
        assert_eq!(t.line_of(5), 1);
        assert_eq!(t.copy_lines(0, 1, 100), ("one\ntwo".into(), false));

        let t = file("b.txt", b"a\nb\n");
        assert_eq!(t.line_count(), 2);
        assert_eq!(t.line(1), b"b");
        let t = file("c.txt", b"");
        assert_eq!(t.line_count(), 0);
        let t = file("d.txt", b"\n");
        assert_eq!(t.line_count(), 1);
        assert_eq!(t.line(0), b"");
    }

    #[test]
    fn index_matches_sequential_across_chunks() {
        let mut content = Vec::new();
        for i in 0..400_000 {
            content.extend_from_slice(format!("line {i} {}\n", "x".repeat(i % 37)).as_bytes());
        }
        let t = file("big.txt", &content);
        assert_eq!(t.line_count(), 400_000);
        assert_eq!(t.line(123_456), format!("line 123456 {}", "x".repeat(123_456 % 37)).as_bytes());
        let p = Pattern::new("LINE 39999", false).unwrap(); // capitals: case-sensitive, no hits
        let (hits, _) = find_all(&t, &p, 1000, &Progress::default()).unwrap();
        assert!(hits.is_empty());
        let p = Pattern::new("line 39999", false).unwrap();
        let (hits, cut) = find_all(&t, &p, 1000, &Progress::default()).unwrap();
        assert!(!cut);
        assert_eq!(hits.len(), 11); // 39999 and 399990..399999
        assert_eq!(t.line_of(hits[0].0), 39999);
        let p = Pattern::new(r"^line \d+5 x{36}$", true).unwrap();
        let (hits, _) = find_all(&t, &p, usize::MAX, &Progress::default()).unwrap();
        let expected = (0..400_000).filter(|i| i % 10 == 5 && i % 37 == 36).count();
        assert_eq!(hits.len(), expected);
        let (hits, cut) = find_all(&t, &Pattern::new("line", false).unwrap(), 10, &Progress::default()).unwrap();
        assert!(cut);
        assert_eq!(hits.len(), 10);
    }

    #[test]
    fn window_is_bounded() {
        let s = display_window(b"a\tb", 0, 10);
        assert_eq!(s, "a   b");
        let long = vec![b'z'; 10_000_000];
        let s = display_window(&long, 5, 20);
        assert_eq!(s.len(), 20);
    }
}
