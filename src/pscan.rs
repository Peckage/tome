//! Parallel record scanner.
//!
//! The document is cut into chunks that are tokenized at the same time. A
//! chunk (other than the first) starts at a guessed tag boundary: the first
//! `<` after its nominal start that looks like markup. Each chunk records
//!
//! * end tags it could not match locally (they close elements opened in an
//!   earlier chunk),
//! * outermost records that start and end inside it, assuming no record was
//!   open when it began,
//! * elements still open at its end.
//!
//! A sequential merge then replays those summaries with the real nesting
//! context. A guess is confirmed when the previous chunk's last token ends
//! exactly at it; otherwise (a `<` inside a comment, CDATA or PI) that chunk
//! is rescanned from the true position. The result is identical to the
//! sequential [`crate::xml::Scanner`], which the tests check.

use rayon::prelude::*;

use crate::xml::{self, Entities, Lexer, Tok, XmlError};

pub struct ScanResult {
    /// Every outermost record, in file order, up to the first error.
    pub ranges: Vec<(usize, usize)>,
    pub error: Option<XmlError>,
}

#[derive(Default)]
struct Chunk {
    start: usize,
    /// Lexer position after the last token (>= the next chunk's start).
    end: usize,
    /// Unmatched end tags: (name, tag start, tag end).
    closes: Vec<((usize, usize), usize, usize)>,
    records: Vec<(usize, usize)>,
    /// Still open at the end: (name, tag start).
    opens: Vec<((usize, usize), usize)>,
    doctypes: Vec<usize>,
    /// First offset at which the local stack became empty again. Only
    /// meaningful for a chunk that starts with nothing open (the first one):
    /// there it is the end of the root element.
    local_root_end: Option<usize>,
    error: Option<XmlError>,
}

struct Ctx<'d> {
    doc: &'d [u8],
    origin: usize,
    tag: &'d [u8],
    entities: &'d Entities,
}

impl Ctx<'_> {
    #[inline]
    fn is_record(&self, name: (usize, usize)) -> bool {
        let n = &self.doc[name.0..name.1];
        let local = match memchr::memrchr(b':', n) {
            Some(i) => &n[i + 1..],
            None => n,
        };
        local == self.tag
    }

    fn scan_chunk(&self, from: usize, until: usize) -> Chunk {
        let doc = self.doc;
        let mut lexer = Lexer::starting_at(doc, self.origin, from);
        let mut chunk = Chunk { start: from, ..Chunk::default() };
        let mut stack: Vec<((usize, usize), usize)> = Vec::with_capacity(32);
        let mut record_depth = 0usize;
        let mut record_start = 0usize;
        while lexer.pos < until {
            let tok = match lexer.next() {
                Ok(Some(t)) => t,
                Ok(None) => break,
                Err(e) => {
                    chunk.error = Some(e);
                    break;
                }
            };
            let step = (|| -> Result<(), XmlError> {
                match tok {
                    Tok::Text { start, end } => {
                        if record_depth == 0 {
                            xml::decode_text(doc, start, end, self.entities)?;
                        }
                    }
                    Tok::Start { start, name, attrs, empty, .. } => {
                        let record = self.is_record(name);
                        if record_depth == 0 {
                            xml::check_attrs(doc, attrs, self.entities)?;
                            if record {
                                record_start = start;
                            }
                        }
                        if empty {
                            if record && record_depth == 0 {
                                chunk.records.push((start, lexer.pos));
                            }
                            if stack.is_empty() && chunk.local_root_end.is_none() {
                                chunk.local_root_end = Some(lexer.pos);
                            }
                        } else {
                            stack.push((name, start));
                            record_depth += record as usize;
                        }
                    }
                    Tok::EndTag { start, name, end } => match stack.pop() {
                        None => chunk.closes.push((name, start, end)),
                        Some((open, _)) => {
                            if doc[open.0..open.1] != doc[name.0..name.1] {
                                return Err(XmlError::new(start, "mismatched tag"));
                            }
                            if self.is_record(name) {
                                record_depth -= 1;
                                if record_depth == 0 {
                                    chunk.records.push((record_start, end));
                                }
                            }
                            if stack.is_empty() && chunk.local_root_end.is_none() {
                                chunk.local_root_end = Some(end);
                            }
                        }
                    },
                    Tok::Doctype { start, .. } => chunk.doctypes.push(start),
                    Tok::CData { .. } | Tok::Skip { .. } => {}
                }
                Ok(())
            })();
            if let Err(e) = step {
                chunk.error = Some(e);
                break;
            }
        }
        chunk.end = lexer.pos;
        chunk.opens = stack;
        chunk
    }
}

/// Plausible start of markup at or after `from`.
fn sync_point(doc: &[u8], from: usize) -> usize {
    let mut i = from;
    while let Some(k) = memchr::memchr(b'<', &doc[i..]) {
        let p = i + k;
        match doc.get(p + 1) {
            Some(&b) if b == b'/' || b == b'!' || b == b'?' || b.is_ascii_alphabetic() || b == b'_' || b >= 0x80 => {
                return p;
            }
            Some(_) => i = p + 1,
            None => return doc.len(),
        }
    }
    doc.len()
}

pub fn scan(doc: &[u8], origin: usize, tag: &str, entities: &Entities, chunk_size: usize) -> ScanResult {
    let ctx = Ctx { doc, origin, tag: tag.as_bytes(), entities };

    // Prolog: everything before the root element, checked sequentially.
    let mut lexer = Lexer::new(doc, origin);
    let root_start = loop {
        match lexer.next() {
            Ok(Some(Tok::Start { start, .. })) => break start,
            Ok(Some(Tok::Text { start, end })) => {
                if let Some(i) = doc[start..end].iter().position(|&b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r')) {
                    return fail(XmlError::new(start + i, "syntax error"));
                }
            }
            Ok(Some(Tok::CData { start, .. } | Tok::EndTag { start, .. })) => {
                return fail(XmlError::new(start, "syntax error"));
            }
            Ok(Some(_)) => {}
            Ok(None) => return fail(XmlError::new(doc.len(), "no element found")),
            Err(e) => return fail(e),
        }
    };

    // Chunk boundaries.
    let mut starts = vec![root_start];
    let mut at = root_start + chunk_size;
    while at < doc.len() {
        let p = sync_point(doc, at);
        if p >= doc.len() {
            break;
        }
        if p > *starts.last().unwrap() {
            starts.push(p);
        }
        at = p + chunk_size;
    }
    let mut ends: Vec<usize> = starts[1..].to_vec();
    ends.push(doc.len());

    let mut chunks: Vec<Chunk> = starts
        .par_iter()
        .zip(ends.par_iter())
        .map(|(&from, &until)| ctx.scan_chunk(from, until))
        .collect();

    // Merge with the real context.
    let mut ranges = Vec::new();
    let mut stack: Vec<((usize, usize), usize)> = Vec::new();
    let mut record_depth = 0usize;
    let mut pending_record = 0usize;
    let mut root_end: Option<usize> = None;
    let mut position = root_start;

    for i in 0..chunks.len() {
        if chunks[i].start != position {
            // Wrong guess: rescan this chunk from where the previous one ended.
            chunks[i] = ctx.scan_chunk(position, ends[i]);
        }
        let chunk = std::mem::take(&mut chunks[i]);
        if let Some(&d) = chunk.doctypes.first() {
            return ScanResult { ranges, error: Some(XmlError::new(d, "syntax error")) };
        }

        let depth_at_start = record_depth;
        let stack_empty_at_start = stack.is_empty();
        let mut cut: Option<usize> = None;
        for &(name, start, end) in &chunk.closes {
            let Some((open, _)) = stack.pop() else {
                return ScanResult { ranges, error: Some(XmlError::new(start, "not well-formed (invalid token)")) };
            };
            if doc[open.0..open.1] != doc[name.0..name.1] {
                return ScanResult { ranges, error: Some(XmlError::new(start, "mismatched tag")) };
            }
            if ctx.is_record(name) {
                record_depth -= 1;
                if record_depth == 0 {
                    ranges.push((pending_record, end));
                    cut = Some(end);
                }
            }
            if stack.is_empty() {
                root_end = Some(end);
                break;
            }
        }
        if stack_empty_at_start {
            root_end = chunk.local_root_end;
        }
        let keep_from = if depth_at_start == 0 { Some(0) } else { cut };
        if let Some(from) = keep_from {
            ranges.extend(chunk.records.iter().copied().filter(|r| r.0 >= from));
        }
        if root_end.is_some() {
            // Everything after the root element is checked as the epilog.
            break;
        }
        if let Some(e) = chunk.error {
            return ScanResult { ranges, error: Some(e) };
        }
        for &(name, start) in &chunk.opens {
            if ctx.is_record(name) {
                if record_depth == 0 {
                    pending_record = start;
                }
                record_depth += 1;
            }
            stack.push((name, start));
        }
        position = chunk.end;
    }

    let Some(root_end) = root_end else {
        return ScanResult {
            ranges,
            error: Some(XmlError::new(doc.len(), "no element found (file ends inside an element)")),
        };
    };
    ranges.retain(|r| r.1 <= root_end);

    // Epilog: only whitespace, comments and processing instructions.
    let mut lexer = Lexer::starting_at(doc, origin, root_end);
    loop {
        match lexer.next() {
            Ok(None) => break,
            Ok(Some(Tok::Skip { .. })) => {}
            Ok(Some(Tok::Text { start, end })) => {
                if let Some(i) = doc[start..end].iter().position(|&b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r')) {
                    return ScanResult { ranges, error: Some(XmlError::new(start + i, "junk after document element")) };
                }
            }
            Ok(Some(Tok::Start { start, .. } | Tok::EndTag { start, .. } | Tok::CData { start, .. } | Tok::Doctype { start, .. })) => {
                return ScanResult { ranges, error: Some(XmlError::new(start, "junk after document element")) };
            }
            Err(e) => return ScanResult { ranges, error: Some(e) },
        }
    }
    ScanResult { ranges, error: None }
}

fn fail(e: XmlError) -> ScanResult {
    ScanResult { ranges: Vec::new(), error: Some(e) }
}
