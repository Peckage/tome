//! A small, fast, non-validating XML tokenizer and record parser.
//!
//! The work is split in two so it can run in parallel:
//!
//! * [`Scanner`] walks the whole document sequentially but only looks at
//!   markup boundaries (memchr-accelerated). It checks nesting and yields the
//!   byte range of every outermost record element.
//! * [`parse_record`] fully parses one record range (attributes, entities,
//!   text) into an [`Obj`]. Ranges are independent, so many threads can do
//!   this at once.
//!
//! Behaviour mirrors Python's ElementTree/expat where it affects output:
//! CRLF/CR become LF, tabs/newlines in attribute values become spaces,
//! comments and processing instructions are skipped, CDATA is plain text,
//! `xmlns` attributes are dropped and namespace prefixes are removed.
//! Character validity (UTF-8, forbidden control characters) is checked once
//! for the whole document in [`crate::engine`], not here.

use std::borrow::Cow;
use std::collections::HashMap;

use memchr::{memchr, memchr2, memchr3, memmem};

#[derive(Debug, Clone)]
pub struct XmlError {
    pub offset: usize,
    pub message: String,
}

impl XmlError {
    pub fn new(offset: usize, message: impl Into<String>) -> Self {
        Self { offset, message: message.into() }
    }
}

pub type XResult<T> = Result<T, XmlError>;

// ---------------------------------------------------------------------------
// Output model
// ---------------------------------------------------------------------------

/// One converted element: attributes, then `#text`, then child groups, in the
/// same key order Python's dict would have.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Obj<'a> {
    pub entries: Vec<(&'a str, Value<'a>)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value<'a> {
    Str(Cow<'a, str>),
    List(Vec<Obj<'a>>),
}

impl<'a> Obj<'a> {
    fn find(&self, key: &str) -> Option<usize> {
        self.entries.iter().position(|(k, _)| *k == key)
    }
}

/// General entities declared in the document's internal DTD subset.
pub type Entities = HashMap<String, String>;

// ---------------------------------------------------------------------------
// Byte classes
// ---------------------------------------------------------------------------

#[inline]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

#[inline]
fn is_name_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b':' || b >= 0x80
}

#[inline]
fn is_name_char(b: u8) -> bool {
    is_name_start(b) || b.is_ascii_digit() || b == b'-' || b == b'.'
}

/// Part after the last ':' (namespace prefix removed).
#[inline]
pub fn local_name(name: &str) -> &str {
    match name.rfind(':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

/// The document is validated as UTF-8 before any parsing, and every slice
/// boundary used here sits next to an ASCII delimiter, so slices are valid.
#[inline]
fn s(doc: &[u8], start: usize, end: usize) -> &str {
    debug_assert!(std::str::from_utf8(&doc[start..end]).is_ok());
    unsafe { std::str::from_utf8_unchecked(&doc[start..end]) }
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum Tok {
    Text { start: usize, end: usize },
    /// `<name attrs>` or `<name attrs/>`. `attrs` is the range between the
    /// name and the closing `>` or `/>`; `end` is just past the `>`.
    Start { start: usize, name: (usize, usize), attrs: (usize, usize), end: usize, empty: bool },
    EndTag { start: usize, name: (usize, usize), end: usize },
    CData { start: usize, content: (usize, usize), end: usize },
    Doctype { start: usize, end: usize },
    /// Comment, processing instruction or XML declaration: no content.
    Skip { #[allow(dead_code)] start: usize, end: usize },
}

pub struct Lexer<'d> {
    doc: &'d [u8],
    pub pos: usize,
    /// Offset where the document proper starts (after a BOM).
    origin: usize,
}

impl<'d> Lexer<'d> {
    pub fn new(doc: &'d [u8], pos: usize) -> Self {
        Self { doc, pos, origin: pos }
    }

    pub fn starting_at(doc: &'d [u8], origin: usize, pos: usize) -> Self {
        Self { doc, pos, origin }
    }

    #[inline]
    fn find(&self, from: usize, needle: &[u8]) -> Option<usize> {
        memmem::find(&self.doc[from..], needle).map(|i| i + from)
    }

    pub fn next(&mut self) -> XResult<Option<Tok>> {
        let doc = self.doc;
        let start = self.pos;
        if start >= doc.len() {
            return Ok(None);
        }
        if doc[start] != b'<' {
            let end = memchr(b'<', &doc[start..]).map_or(doc.len(), |i| i + start);
            self.pos = end;
            return Ok(Some(Tok::Text { start, end }));
        }

        let next = *doc.get(start + 1).ok_or_else(|| XmlError::new(start, "unclosed token"))?;
        let tok = match next {
            b'/' => {
                let name_start = start + 2;
                let mut i = name_start;
                while i < doc.len() && is_name_char(doc[i]) {
                    i += 1;
                }
                if i == name_start || !is_name_start(doc[name_start]) {
                    return Err(XmlError::new(start, "not well-formed (invalid token)"));
                }
                let name = (name_start, i);
                while i < doc.len() && is_ws(doc[i]) {
                    i += 1;
                }
                if i >= doc.len() {
                    return Err(XmlError::new(start, "unclosed token"));
                }
                if doc[i] != b'>' {
                    return Err(XmlError::new(i, "not well-formed (invalid token)"));
                }
                Tok::EndTag { start, name, end: i + 1 }
            }
            b'?' => {
                let close = self
                    .find(start + 2, b"?>")
                    .ok_or_else(|| XmlError::new(start, "unclosed token"))?;
                let mut t = start + 2;
                while t < close && is_name_char(doc[t]) {
                    t += 1;
                }
                let target = &doc[start + 2..t];
                if t == start + 2 {
                    return Err(XmlError::new(start, "not well-formed (invalid token)"));
                }
                if target.eq_ignore_ascii_case(b"xml") && (start != self.origin || target != b"xml")
                {
                    return Err(XmlError::new(
                        start,
                        "XML or text declaration not at start of entity",
                    ));
                }
                Tok::Skip { start, end: close + 2 }
            }
            b'!' => {
                let rest = &doc[start..];
                if rest.starts_with(b"<!--") {
                    let close = self
                        .find(start + 4, b"--")
                        .ok_or_else(|| XmlError::new(start, "unclosed token"))?;
                    if doc.get(close + 2) != Some(&b'>') {
                        return Err(XmlError::new(close, "not well-formed (invalid token)"));
                    }
                    Tok::Skip { start, end: close + 3 }
                } else if rest.starts_with(b"<![CDATA[") {
                    let close = self
                        .find(start + 9, b"]]>")
                        .ok_or_else(|| XmlError::new(start, "unclosed CDATA section"))?;
                    Tok::CData { start, content: (start + 9, close), end: close + 3 }
                } else if rest.starts_with(b"<!DOCTYPE") {
                    let end = doctype_end(doc, start)?;
                    Tok::Doctype { start, end }
                } else {
                    return Err(XmlError::new(start, "not well-formed (invalid token)"));
                }
            }
            b if is_name_start(b) => {
                let name_start = start + 1;
                let mut i = name_start + 1;
                while i < doc.len() && is_name_char(doc[i]) {
                    i += 1;
                }
                let name = (name_start, i);
                // Find the closing '>' while skipping quoted attribute values.
                let mut j = i;
                let gt = loop {
                    let k = memchr3(b'>', b'"', b'\'', &doc[j..])
                        .ok_or_else(|| XmlError::new(start, "unclosed token"))?
                        + j;
                    match doc[k] {
                        b'>' => break k,
                        q => {
                            let close = memchr(q, &doc[k + 1..])
                                .ok_or_else(|| XmlError::new(k, "unclosed token"))?;
                            j = k + 1 + close + 1;
                        }
                    }
                };
                let empty = gt > i && doc[gt - 1] == b'/';
                let attrs_end = if empty { gt - 1 } else { gt };
                Tok::Start { start, name, attrs: (i, attrs_end), end: gt + 1, empty }
            }
            _ => return Err(XmlError::new(start, "not well-formed (invalid token)")),
        };
        self.pos = match tok {
            Tok::Start { end, .. }
            | Tok::EndTag { end, .. }
            | Tok::CData { end, .. }
            | Tok::Doctype { end, .. }
            | Tok::Skip { end, .. } => end,
            Tok::Text { end, .. } => end,
        };
        Ok(Some(tok))
    }
}

fn doctype_end(doc: &[u8], start: usize) -> XResult<usize> {
    let mut i = start + 9;
    let mut in_subset = false;
    while i < doc.len() {
        match doc[i] {
            b'"' | b'\'' => {
                let q = doc[i];
                i += memchr(q, &doc[i + 1..]).ok_or_else(|| XmlError::new(i, "unclosed token"))? + 2;
                continue;
            }
            b'[' if !in_subset => in_subset = true,
            b']' if in_subset => in_subset = false,
            b'<' if in_subset && doc[i..].starts_with(b"<!--") => {
                i += memmem::find(&doc[i..], b"-->")
                    .ok_or_else(|| XmlError::new(i, "unclosed token"))?
                    + 3;
                continue;
            }
            b'>' if !in_subset => return Ok(i + 1),
            _ => {}
        }
        i += 1;
    }
    Err(XmlError::new(start, "unclosed token"))
}

/// Collect `<!ENTITY name "value">` declarations from a DOCTYPE's internal
/// subset. Parameter entities are ignored; external entities are recorded
/// with a marker so references to them fail with a clear message.
pub fn parse_entities(doc: &[u8], start: usize, end: usize, into: &mut Entities) {
    let text = s(doc, start, end);
    let mut rest = text;
    while let Some(i) = rest.find("<!ENTITY") {
        rest = &rest[i + 8..];
        let body = rest.trim_start();
        if body.starts_with('%') {
            continue;
        }
        let name_len = body
            .bytes()
            .position(|b| !is_name_char(b))
            .unwrap_or(body.len());
        let name = &body[..name_len];
        let after = body[name_len..].trim_start();
        let value = match after.as_bytes().first() {
            Some(&q @ (b'"' | b'\'')) => after[1..]
                .find(q as char)
                .map(|j| after[1..1 + j].to_string()),
            _ => Some(EXTERNAL_ENTITY.to_string()),
        };
        if let Some(value) = value {
            into.entry(name.to_string()).or_insert(value);
        }
    }
}

const EXTERNAL_ENTITY: &str = "\u{0}external";

// ---------------------------------------------------------------------------
// Scanner: finds record ranges and checks document structure
// ---------------------------------------------------------------------------

pub struct Scanner<'d> {
    lexer: Lexer<'d>,
    doc: &'d [u8],
    tag: Vec<u8>,
    stack: Vec<(usize, usize)>,
    record_depth: usize,
    record_start: usize,
    root_seen: bool,
    root_closed: bool,
    entities: &'d Entities,
}

impl<'d> Scanner<'d> {
    pub fn new(doc: &'d [u8], origin: usize, tag: &str, entities: &'d Entities) -> Self {
        Self {
            lexer: Lexer::new(doc, origin),
            doc,
            tag: tag.as_bytes().to_vec(),
            stack: Vec::with_capacity(32),
            record_depth: 0,
            record_start: 0,
            root_seen: false,
            root_closed: false,
            entities,
        }
    }

    #[inline]
    fn is_record(&self, name: (usize, usize)) -> bool {
        let n = &self.doc[name.0..name.1];
        let local = match memchr::memrchr(b':', n) {
            Some(i) => &n[i + 1..],
            None => n,
        };
        local == self.tag.as_slice()
    }

    /// Next outermost record as a byte range `[start, end)`.
    pub fn next_record(&mut self) -> XResult<Option<(usize, usize)>> {
        let doc = self.doc;
        while let Some(tok) = self.lexer.next()? {
            match tok {
                Tok::Text { start, end } => {
                    if self.stack.is_empty() {
                        if let Some(i) = doc[start..end].iter().position(|&b| !is_ws(b)) {
                            let msg = if self.root_closed {
                                "junk after document element"
                            } else {
                                "syntax error"
                            };
                            return Err(XmlError::new(start + i, msg));
                        }
                    } else if self.record_depth == 0 {
                        // Text outside records is not converted, but its
                        // entity references must still be valid.
                        decode_text(doc, start, end, self.entities)?;
                    }
                }
                Tok::Start { start, name, attrs, empty, .. } => {
                    if self.stack.is_empty() {
                        if self.root_closed {
                            return Err(XmlError::new(start, "junk after document element"));
                        }
                        self.root_seen = true;
                    }
                    let record = self.is_record(name);
                    if self.record_depth == 0 {
                        // Not part of any record: still check attribute syntax.
                        for_each_attr(doc, attrs, self.entities, |_, _| Ok(()))?;
                        if record {
                            self.record_start = start;
                        }
                    }
                    if empty {
                        if self.stack.is_empty() {
                            self.root_closed = true;
                        }
                        if record && self.record_depth == 0 {
                            return Ok(Some((start, self.lexer.pos)));
                        }
                    } else {
                        self.stack.push(name);
                        self.record_depth += record as usize;
                    }
                }
                Tok::EndTag { start, name, end } => {
                    let open = self
                        .stack
                        .pop()
                        .ok_or_else(|| XmlError::new(start, "not well-formed (invalid token)"))?;
                    if doc[open.0..open.1] != doc[name.0..name.1] {
                        return Err(XmlError::new(start, "mismatched tag"));
                    }
                    if self.stack.is_empty() {
                        self.root_closed = true;
                    }
                    if self.is_record(name) {
                        self.record_depth -= 1;
                        if self.record_depth == 0 {
                            return Ok(Some((self.record_start, end)));
                        }
                    }
                }
                Tok::CData { start, .. } => {
                    if self.stack.is_empty() {
                        return Err(XmlError::new(start, "syntax error"));
                    }
                }
                Tok::Doctype { start, .. } => {
                    if self.root_seen {
                        return Err(XmlError::new(start, "syntax error"));
                    }
                }
                Tok::Skip { .. } => {}
            }
        }
        if !self.stack.is_empty() {
            return Err(XmlError::new(doc.len(), "no element found (file ends inside an element)"));
        }
        if !self.root_seen {
            return Err(XmlError::new(doc.len(), "no element found"));
        }
        Ok(None)
    }
}

/// Read the prolog (everything before the root element) and return the
/// entities declared in its DOCTYPE, if any.
pub fn read_prolog(doc: &[u8], origin: usize) -> XResult<Entities> {
    let mut entities = Entities::new();
    let mut lexer = Lexer::new(doc, origin);
    while let Some(tok) = lexer.next()? {
        match tok {
            Tok::Doctype { start, end } => parse_entities(doc, start, end, &mut entities),
            Tok::Start { .. } => break,
            _ => {}
        }
    }
    Ok(entities)
}

/// Guess the record element: the shallowest name below the root that repeats
/// within the first `max_tokens` tokens.
pub fn guess_record_tag(doc: &[u8], origin: usize, max_tokens: usize) -> Option<String> {
    let mut lexer = Lexer::new(doc, origin);
    let mut depth = 0usize;
    let mut levels: Vec<Vec<(String, usize)>> = Vec::new();
    for _ in 0..max_tokens {
        match lexer.next() {
            Ok(Some(Tok::Start { name, empty, .. })) => {
                depth += 1;
                if depth >= 2 {
                    let level = depth - 2;
                    if levels.len() <= level {
                        levels.resize_with(level + 1, Vec::new);
                    }
                    let n = local_name(s(doc, name.0, name.1));
                    match levels[level].iter_mut().find(|(k, _)| k == n) {
                        Some((_, c)) => *c += 1,
                        None => levels[level].push((n.to_string(), 1)),
                    }
                }
                if empty {
                    depth -= 1;
                }
            }
            Ok(Some(Tok::EndTag { .. })) => depth = depth.saturating_sub(1),
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    let best = |level: &Vec<(String, usize)>| {
        let mut best: Option<&(String, usize)> = None;
        for item in level {
            if best.is_none_or(|b| item.1 > b.1) {
                best = Some(item);
            }
        }
        best.cloned()
    };
    for level in &levels {
        if let Some((name, count)) = best(level) {
            if count > 1 {
                return Some(name);
            }
        }
    }
    levels.first().and_then(best).map(|(n, _)| n)
}

// ---------------------------------------------------------------------------
// Text, references and attributes
// ---------------------------------------------------------------------------

fn char_from_ref(body: &str, offset: usize) -> XResult<char> {
    let code = if let Some(hex) = body.strip_prefix("#x") {
        u32::from_str_radix(hex, 16).ok()
    } else if let Some(dec) = body.strip_prefix('#') {
        if dec.bytes().all(|b| b.is_ascii_digit()) { dec.parse::<u32>().ok() } else { None }
    } else {
        None
    };
    let c = code
        .and_then(char::from_u32)
        .ok_or_else(|| XmlError::new(offset, "reference to invalid character number"))?;
    let ok = matches!(c, '\t' | '\n' | '\r')
        || matches!(c, '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}' | '\u{10000}'..='\u{10FFFF}');
    if !ok {
        return Err(XmlError::new(offset, "reference to invalid character number"));
    }
    Ok(c)
}

/// Append the expansion of the reference `&body;` at `offset` to `out`.
fn push_ref(
    out: &mut String,
    body: &str,
    offset: usize,
    entities: &Entities,
    depth: usize,
) -> XResult<()> {
    match body {
        "lt" => out.push('<'),
        "gt" => out.push('>'),
        "amp" => out.push('&'),
        "apos" => out.push('\''),
        "quot" => out.push('"'),
        b if b.starts_with('#') => out.push(char_from_ref(b, offset)?),
        name => {
            if name.is_empty() || !name.bytes().all(is_name_char) {
                return Err(XmlError::new(offset, "not well-formed (invalid token)"));
            }
            let value = entities
                .get(name)
                .ok_or_else(|| XmlError::new(offset, format!("undefined entity &{name};")))?;
            if value == EXTERNAL_ENTITY {
                return Err(XmlError::new(offset, format!("external entity &{name}; is not supported")));
            }
            if depth > 16 || out.len() > 16 * 1024 * 1024 {
                return Err(XmlError::new(offset, "entity expansion too deep or too large"));
            }
            if value.contains('<') {
                return Err(XmlError::new(offset, format!("entity &{name}; contains markup, which is not supported")));
            }
            expand_into(out, value, offset, entities, depth + 1)?;
        }
    }
    Ok(())
}

fn expand_into(out: &mut String, text: &str, offset: usize, entities: &Entities, depth: usize) -> XResult<()> {
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let semi = after
            .find(';')
            .ok_or_else(|| XmlError::new(offset, "not well-formed (invalid token)"))?;
        push_ref(out, &after[..semi], offset, entities, depth)?;
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    Ok(())
}

/// Check the attribute syntax of a start tag without keeping the values.
pub fn check_attrs(doc: &[u8], attrs: (usize, usize), entities: &Entities) -> XResult<()> {
    for_each_attr(doc, attrs, entities, |_, _| Ok(()))
}

/// Decode character data `[start, end)`: references expanded, CRLF and CR
/// turned into LF. Borrows when there is nothing to change.
pub fn decode_text<'a>(doc: &'a [u8], start: usize, end: usize, entities: &Entities) -> XResult<Cow<'a, str>> {
    let raw = &doc[start..end];
    if let Some(i) = memmem::find(raw, b"]]>") {
        return Err(XmlError::new(start + i, "not well-formed (invalid token)"));
    }
    if memchr2(b'&', b'\r', raw).is_none() {
        return Ok(Cow::Borrowed(s(doc, start, end)));
    }
    let mut out = String::with_capacity(raw.len());
    let mut i = start;
    while i < end {
        let Some(k) = memchr2(b'&', b'\r', &doc[i..end]) else {
            out.push_str(s(doc, i, end));
            break;
        };
        let k = k + i;
        out.push_str(s(doc, i, k));
        if doc[k] == b'\r' {
            out.push('\n');
            i = if doc.get(k + 1) == Some(&b'\n') && k + 1 < end { k + 2 } else { k + 1 };
        } else {
            let semi = memchr(b';', &doc[k..end])
                .ok_or_else(|| XmlError::new(k, "not well-formed (invalid token)"))?
                + k;
            push_ref(&mut out, s(doc, k + 1, semi), k, entities, 0)?;
            i = semi + 1;
        }
    }
    Ok(Cow::Owned(out))
}

/// Normalize and decode an attribute value: literal tab, CR, LF and CRLF
/// become one space each; references are expanded afterwards.
fn decode_attr<'a>(doc: &'a [u8], start: usize, end: usize, entities: &Entities) -> XResult<Cow<'a, str>> {
    let raw = &doc[start..end];
    if let Some(i) = memchr(b'<', raw) {
        return Err(XmlError::new(start + i, "not well-formed (invalid token)"));
    }
    let needs_work = raw.iter().any(|&b| matches!(b, b'&' | b'\t' | b'\n' | b'\r'));
    if !needs_work {
        return Ok(Cow::Borrowed(s(doc, start, end)));
    }
    let mut out = String::with_capacity(raw.len());
    let mut i = start;
    let mut plain = start;
    while i < end {
        match doc[i] {
            b'\t' | b'\n' => {
                out.push_str(s(doc, plain, i));
                out.push(' ');
                i += 1;
                plain = i;
            }
            b'\r' => {
                out.push_str(s(doc, plain, i));
                out.push(' ');
                i += if i + 1 < end && doc[i + 1] == b'\n' { 2 } else { 1 };
                plain = i;
            }
            b'&' => {
                out.push_str(s(doc, plain, i));
                let semi = memchr(b';', &doc[i..end])
                    .ok_or_else(|| XmlError::new(i, "not well-formed (invalid token)"))?
                    + i;
                push_ref(&mut out, s(doc, i + 1, semi), i, entities, 0)?;
                i = semi + 1;
                plain = i;
            }
            _ => i += 1,
        }
    }
    out.push_str(s(doc, plain, end));
    Ok(Cow::Owned(out))
}

/// Parse the attributes in `attrs` and call `f(qualified_name, value)` for
/// each one except namespace declarations. Rejects syntax errors and
/// duplicate attribute names.
fn for_each_attr<'a>(
    doc: &'a [u8],
    attrs: (usize, usize),
    entities: &Entities,
    mut f: impl FnMut(&'a str, Cow<'a, str>) -> XResult<()>,
) -> XResult<()> {
    let (mut i, end) = attrs;
    let mut seen: [(usize, usize); 16] = [(0, 0); 16];
    let mut seen_len = 0usize;
    let mut seen_more: Vec<(usize, usize)> = Vec::new();
    loop {
        let before = i;
        while i < end && is_ws(doc[i]) {
            i += 1;
        }
        if i >= end {
            return Ok(());
        }
        if i == before || !is_name_start(doc[i]) {
            return Err(XmlError::new(i, "not well-formed (invalid token)"));
        }
        let name_start = i;
        while i < end && is_name_char(doc[i]) {
            i += 1;
        }
        let name = (name_start, i);
        while i < end && is_ws(doc[i]) {
            i += 1;
        }
        if i >= end || doc[i] != b'=' {
            return Err(XmlError::new(i, "not well-formed (invalid token)"));
        }
        i += 1;
        while i < end && is_ws(doc[i]) {
            i += 1;
        }
        if i >= end || !matches!(doc[i], b'"' | b'\'') {
            return Err(XmlError::new(i, "not well-formed (invalid token)"));
        }
        let q = doc[i];
        let value_start = i + 1;
        let value_end = memchr(q, &doc[value_start..end])
            .ok_or_else(|| XmlError::new(i, "not well-formed (invalid token)"))?
            + value_start;
        i = value_end + 1;

        let qname = &doc[name.0..name.1];
        let dup = seen[..seen_len]
            .iter()
            .chain(seen_more.iter())
            .any(|&(a, b)| &doc[a..b] == qname);
        if dup {
            return Err(XmlError::new(name_start, "duplicate attribute"));
        }
        if seen_len < seen.len() {
            seen[seen_len] = name;
            seen_len += 1;
        } else {
            seen_more.push(name);
        }

        let value = decode_attr(doc, value_start, value_end, entities)?;
        if qname == b"xmlns" || qname.starts_with(b"xmlns:") {
            continue;
        }
        f(s(doc, name.0, name.1), value)?;
    }
}

// ---------------------------------------------------------------------------
// Record parser
// ---------------------------------------------------------------------------

struct Frame<'a> {
    obj: Obj<'a>,
    key: &'a str,
    name: (usize, usize),
    text: Option<Cow<'a, str>>,
    has_child: bool,
}

fn finish_text<'a>(frame: &mut Frame<'a>) {
    if frame.has_child {
        return;
    }
    frame.has_child = true; // text after the first child is tail text: ignored
    if let Some(text) = frame.text.take() {
        let trimmed: Cow<'a, str> = match text {
            Cow::Borrowed(t) => Cow::Borrowed(t.trim()),
            Cow::Owned(t) => {
                let tr = t.trim();
                if tr.len() == t.len() { Cow::Owned(t) } else { Cow::Owned(tr.to_string()) }
            }
        };
        if !trimmed.is_empty() {
            frame.obj.entries.push(("#text", Value::Str(trimmed)));
        }
    }
}

fn add_text<'a>(frame: &mut Frame<'a>, piece: Cow<'a, str>) {
    if frame.has_child {
        return;
    }
    frame.text = Some(match frame.text.take() {
        None => piece,
        Some(prev) => {
            let mut owned = prev.into_owned();
            owned.push_str(&piece);
            Cow::Owned(owned)
        }
    });
}

fn start_frame<'a>(
    doc: &'a [u8],
    name: (usize, usize),
    attrs: (usize, usize),
    entities: &Entities,
) -> XResult<Frame<'a>> {
    let mut obj = Obj { entries: Vec::new() };
    for_each_attr(doc, attrs, entities, |qname, value| {
        let key = local_name(qname);
        match obj.find(key) {
            // Two prefixed attributes with the same local name: the later
            // value wins, in the earlier position, like a Python dict.
            Some(i) => obj.entries[i].1 = Value::Str(value),
            None => obj.entries.push((key, Value::Str(value))),
        }
        Ok(())
    })?;
    Ok(Frame { obj, key: local_name(s(doc, name.0, name.1)), name, text: None, has_child: false })
}

fn attach<'a>(doc: &'a [u8], parent: &mut Frame<'a>, child: Frame<'a>, at: usize) -> XResult<()> {
    finish_text(parent);
    let key = child.key;
    match parent.obj.find(key) {
        Some(i) => match &mut parent.obj.entries[i].1 {
            Value::List(items) => items.push(child.obj),
            Value::Str(_) => {
                return Err(XmlError::new(
                    at,
                    format!(
                        "<{}> has both an attribute and a child element named '{}'",
                        local_name(s(doc, parent.name.0, parent.name.1)),
                        key
                    ),
                ));
            }
        },
        None => parent.obj.entries.push((key, Value::List(vec![child.obj]))),
    }
    Ok(())
}

/// Fully parse one record range produced by [`Scanner`].
pub fn parse_record<'a>(doc: &'a [u8], range: (usize, usize), entities: &Entities) -> XResult<Obj<'a>> {
    let mut lexer = Lexer::starting_at(doc, usize::MAX, range.0);
    let mut stack: Vec<Frame<'a>> = Vec::with_capacity(8);
    while lexer.pos < range.1 {
        let Some(tok) = lexer.next()? else { break };
        match tok {
            Tok::Start { start, name, attrs, empty, .. } => {
                let mut frame = start_frame(doc, name, attrs, entities)?;
                if empty {
                    finish_text(&mut frame);
                    match stack.last_mut() {
                        Some(parent) => attach(doc, parent, frame, start)?,
                        None => return Ok(frame.obj),
                    }
                } else {
                    stack.push(frame);
                }
            }
            Tok::EndTag { start, .. } => {
                let mut frame = stack.pop().ok_or_else(|| XmlError::new(start, "mismatched tag"))?;
                finish_text(&mut frame);
                match stack.last_mut() {
                    Some(parent) => attach(doc, parent, frame, start)?,
                    None => return Ok(frame.obj),
                }
            }
            Tok::Text { start, end } => {
                if let Some(frame) = stack.last_mut() {
                    if !frame.has_child {
                        let piece = decode_text(doc, start, end, entities)?;
                        add_text(frame, piece);
                    } else {
                        decode_text(doc, start, end, entities)?; // validate only
                    }
                }
            }
            Tok::CData { content, .. } => {
                if let Some(frame) = stack.last_mut() {
                    let raw = &doc[content.0..content.1];
                    let piece = if memchr(b'\r', raw).is_some() {
                        Cow::Owned(s(doc, content.0, content.1).replace("\r\n", "\n").replace('\r', "\n"))
                    } else {
                        Cow::Borrowed(s(doc, content.0, content.1))
                    };
                    add_text(frame, piece);
                }
            }
            Tok::Doctype { start, .. } => return Err(XmlError::new(start, "syntax error")),
            Tok::Skip { .. } => {}
        }
    }
    Err(XmlError::new(range.0, "record ended unexpectedly"))
}
