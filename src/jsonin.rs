//! JSON input: find the records, then parse each one into a [`Node`].
//!
//! Records are the items of the root array, the items of an array found by a
//! key path such as `data.items`, or every top-level value of an NDJSON /
//! concatenated-JSON file. Indexing only skips over values (strings are
//! jumped with memchr); full parsing happens per record, in parallel.
//!
//! The parser keeps numbers exactly as written, keeps key order and
//! duplicate keys, and borrows strings from the file when they contain no
//! escapes.

use std::borrow::Cow;

use crate::node::Node;
use crate::xml::{XResult, XmlError};

const MAX_DEPTH: usize = 4096;

#[inline]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && is_ws(b[i]) {
        i += 1;
    }
    i
}

/// End of the string starting at `b[i] == '"'` (one past the closing quote).
fn skip_string(b: &[u8], i: usize) -> XResult<usize> {
    let mut at = i + 1;
    loop {
        match memchr::memchr2(b'"', b'\\', &b[at.min(b.len())..]) {
            Some(k) => {
                let j = at + k;
                if b[j] == b'"' {
                    return Ok(j + 1);
                }
                at = j + 2; // skip the escaped character
            }
            None => return Err(XmlError::new(i, "unterminated string")),
        }
    }
}

/// End of the value starting at `b[i]` (not whitespace). Structure is only
/// checked loosely here; [`parse`] checks everything.
pub fn skip_value(b: &[u8], i: usize) -> XResult<usize> {
    let Some(&first) = b.get(i) else { return Err(XmlError::new(i, "expected a value")) };
    match first {
        b'"' => skip_string(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = skip_string(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Ok(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            Err(XmlError::new(i, if first == b'{' { "unclosed object" } else { "unclosed array" }))
        }
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b']' | b'}' | b':') && !is_ws(b[j]) {
                j += 1;
            }
            if j == i { Err(XmlError::new(i, "expected a value")) } else { Ok(j) }
        }
    }
}

/// Where the records are.
pub struct Located {
    pub ranges: Vec<(usize, usize)>,
    /// The path that was used: "" for the root array, "data.items", "$" for
    /// the whole document as one record, or "lines" for NDJSON.
    pub path: String,
}

/// Items of the array that starts at `b[i] == '['`.
fn array_items(b: &[u8], i: usize) -> XResult<(Vec<(usize, usize)>, usize)> {
    let mut items = Vec::new();
    let mut j = skip_ws(b, i + 1);
    if b.get(j) == Some(&b']') {
        return Ok((items, j + 1));
    }
    loop {
        let end = skip_value(b, j)?;
        items.push((j, end));
        j = skip_ws(b, end);
        match b.get(j) {
            Some(b',') => j = skip_ws(b, j + 1),
            Some(b']') => return Ok((items, j + 1)),
            _ => return Err(XmlError::new(j, "expected ',' or ']' in array")),
        }
    }
}

/// (key range without quotes, value start, value end) for each member of
/// the object at `b[i] == '{'`.
fn object_members(b: &[u8], i: usize) -> XResult<Vec<((usize, usize), usize, usize)>> {
    let mut members = Vec::new();
    let mut j = skip_ws(b, i + 1);
    if b.get(j) == Some(&b'}') {
        return Ok(members);
    }
    loop {
        if b.get(j) != Some(&b'"') {
            return Err(XmlError::new(j, "expected a key"));
        }
        let key = (j + 1, skip_string(b, j)? - 1);
        j = skip_ws(b, key.1 + 1);
        if b.get(j) != Some(&b':') {
            return Err(XmlError::new(j, "expected ':'"));
        }
        let value = skip_ws(b, j + 1);
        let end = skip_value(b, value)?;
        members.push((key, value, end));
        j = skip_ws(b, end);
        match b.get(j) {
            Some(b',') => j = skip_ws(b, j + 1),
            Some(b'}') => return Ok(members),
            _ => return Err(XmlError::new(j, "expected ',' or '}' in object")),
        }
    }
}

fn key_text(b: &[u8], range: (usize, usize)) -> Cow<'_, str> {
    let raw = &b[range.0..range.1];
    if memchr::memchr(b'\\', raw).is_none() {
        return Cow::Borrowed(std::str::from_utf8(raw).unwrap_or(""));
    }
    let mut p = Parser { b, i: range.0 - 1, depth: 0 };
    match p.string() {
        Ok(s) => s,
        Err(_) => Cow::Borrowed(""),
    }
}

/// Find the records in `b` (from `origin`). `path` is "" for automatic,
/// "$" for the whole document, or dot-separated keys.
pub fn locate(b: &[u8], origin: usize, path: &str) -> XResult<Located> {
    let root = skip_ws(b, origin);
    if root >= b.len() {
        return Err(XmlError::new(root, "no JSON value in this file"));
    }
    let root_end = skip_value(b, root)?;
    let after = skip_ws(b, root_end);
    let path = path.trim();

    // More values after the first: NDJSON or concatenated JSON.
    if after < b.len() && path.is_empty() || path == "lines" {
        let mut ranges = Vec::new();
        let mut j = root;
        while j < b.len() {
            let end = skip_value(b, j)?;
            ranges.push((j, end));
            j = skip_ws(b, end);
        }
        return Ok(Located { ranges, path: "lines".into() });
    }
    if after < b.len() {
        return Err(XmlError::new(after, "extra data after the JSON value"));
    }
    if path == "$" {
        return Ok(Located { ranges: vec![(root, root_end)], path: "$".into() });
    }
    if path.is_empty() {
        return match b[root] {
            b'[' => Ok(Located { ranges: array_items(b, root)?.0, path: String::new() }),
            b'{' => {
                // The biggest array directly under the root holds the records.
                let best = object_members(b, root)?
                    .into_iter()
                    .filter(|(_, v, _)| b[*v] == b'[')
                    .max_by_key(|(_, v, e)| e - v);
                match best {
                    Some((key, v, _)) => {
                        let name = key_text(b, key).into_owned();
                        Ok(Located { ranges: array_items(b, v)?.0, path: name })
                    }
                    None => Ok(Located { ranges: vec![(root, root_end)], path: "$".into() }),
                }
            }
            _ => Ok(Located { ranges: vec![(root, root_end)], path: "$".into() }),
        };
    }

    // An explicit path: walk the keys.
    let mut at = root;
    for segment in path.trim_start_matches("$.").split('.') {
        if b[at] == b'[' {
            if let Ok(n) = segment.parse::<usize>() {
                let (items, _) = array_items(b, at)?;
                at = items.get(n).ok_or_else(|| XmlError::new(at, format!("no item {n} in this array")))?.0;
                continue;
            }
        }
        if b[at] != b'{' {
            return Err(XmlError::new(at, format!("'{segment}' is not under an object")));
        }
        let found = object_members(b, at)?.into_iter().find(|(k, _, _)| key_text(b, *k) == segment);
        match found {
            Some((_, v, _)) => at = v,
            None => return Err(XmlError::new(at, format!("no key '{segment}' here"))),
        }
    }
    let end = skip_value(b, at)?;
    let ranges = if b[at] == b'[' { array_items(b, at)?.0 } else { vec![(at, end)] };
    Ok(Located { ranges, path: path.to_string() })
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn err<T>(&self, message: &str) -> XResult<T> {
        Err(XmlError::new(self.i, message))
    }

    fn ws(&mut self) {
        self.i = skip_ws(self.b, self.i);
    }

    fn value(&mut self) -> XResult<Node<'a>> {
        self.ws();
        let Some(&c) = self.b.get(self.i) else { return self.err("expected a value") };
        match c {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string().map(Node::Str),
            b't' => self.literal(b"true", Node::Bool(true)),
            b'f' => self.literal(b"false", Node::Bool(false)),
            b'n' => self.literal(b"null", Node::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => self.err("expected a value"),
        }
    }

    fn literal(&mut self, word: &[u8], node: Node<'a>) -> XResult<Node<'a>> {
        if self.b[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(node)
        } else {
            self.err("expected a value")
        }
    }

    fn number(&mut self) -> XResult<Node<'a>> {
        let b = self.b;
        let start = self.i;
        let mut j = start;
        if b.get(j) == Some(&b'-') {
            j += 1;
        }
        match b.get(j) {
            Some(b'0') => j += 1,
            Some(b'1'..=b'9') => {
                while b.get(j).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                }
            }
            _ => return Err(XmlError::new(j, "bad number")),
        }
        if b.get(j) == Some(&b'.') {
            j += 1;
            if !b.get(j).is_some_and(u8::is_ascii_digit) {
                return Err(XmlError::new(j, "bad number"));
            }
            while b.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
        }
        if matches!(b.get(j), Some(b'e' | b'E')) {
            j += 1;
            if matches!(b.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            if !b.get(j).is_some_and(u8::is_ascii_digit) {
                return Err(XmlError::new(j, "bad number"));
            }
            while b.get(j).is_some_and(u8::is_ascii_digit) {
                j += 1;
            }
        }
        self.i = j;
        // The input was checked to be UTF-8 and these bytes are ASCII.
        Ok(Node::Num(Cow::Borrowed(std::str::from_utf8(&b[start..j]).unwrap())))
    }

    fn string(&mut self) -> XResult<Cow<'a, str>> {
        let b = self.b;
        let start = self.i + 1;
        let end = skip_string(b, self.i)?; // one past the closing quote
        let raw = &b[start..end - 1];
        if let Some(k) = raw.iter().position(|&c| c < 0x20) {
            return Err(XmlError::new(start + k, "control character in string"));
        }
        self.i = end;
        if memchr::memchr(b'\\', raw).is_none() {
            return Ok(Cow::Borrowed(std::str::from_utf8(raw).map_err(|_| XmlError::new(start, "invalid UTF-8"))?));
        }
        let mut out = String::with_capacity(raw.len());
        let mut k = 0;
        while k < raw.len() {
            let Some(rel) = memchr::memchr(b'\\', &raw[k..]) else {
                out.push_str(std::str::from_utf8(&raw[k..]).map_err(|_| XmlError::new(start, "invalid UTF-8"))?);
                break;
            };
            out.push_str(std::str::from_utf8(&raw[k..k + rel]).map_err(|_| XmlError::new(start, "invalid UTF-8"))?);
            k += rel;
            let at = start + k;
            let esc = *raw.get(k + 1).ok_or_else(|| XmlError::new(at, "bad escape"))?;
            k += 2;
            match esc {
                b'"' => out.push('"'),
                b'\\' => out.push('\\'),
                b'/' => out.push('/'),
                b'b' => out.push('\u{8}'),
                b'f' => out.push('\u{c}'),
                b'n' => out.push('\n'),
                b'r' => out.push('\r'),
                b't' => out.push('\t'),
                b'u' => {
                    let hi = hex4(raw, k).ok_or_else(|| XmlError::new(at, "bad \\u escape"))?;
                    k += 4;
                    let c = if (0xD800..0xDC00).contains(&hi) && raw.get(k) == Some(&b'\\') && raw.get(k + 1) == Some(&b'u') {
                        match hex4(raw, k + 2) {
                            Some(lo) if (0xDC00..0xE000).contains(&lo) => {
                                k += 6;
                                char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))
                            }
                            _ => None,
                        }
                    } else {
                        char::from_u32(hi)
                    };
                    // A lone surrogate cannot be stored as UTF-8.
                    out.push(c.unwrap_or('\u{FFFD}'));
                }
                _ => return Err(XmlError::new(at, "bad escape")),
            }
        }
        Ok(Cow::Owned(out))
    }

    fn enter(&mut self) -> XResult<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH { self.err("nested too deeply") } else { Ok(()) }
    }

    fn array(&mut self) -> XResult<Node<'a>> {
        self.enter()?;
        self.i += 1;
        let mut items = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            self.depth -= 1;
            return Ok(Node::Arr(items));
        }
        loop {
            items.push(self.value()?);
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    self.depth -= 1;
                    return Ok(Node::Arr(items));
                }
                _ => return self.err("expected ',' or ']'"),
            }
        }
    }

    fn object(&mut self) -> XResult<Node<'a>> {
        self.enter()?;
        self.i += 1;
        let mut entries = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            self.depth -= 1;
            return Ok(Node::Obj(entries));
        }
        loop {
            self.ws();
            if self.b.get(self.i) != Some(&b'"') {
                return self.err("expected a key");
            }
            let key = self.string()?;
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return self.err("expected ':'");
            }
            self.i += 1;
            let value = self.value()?;
            entries.push((key, value));
            self.ws();
            match self.b.get(self.i) {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    self.depth -= 1;
                    return Ok(Node::Obj(entries));
                }
                _ => return self.err("expected ',' or '}'"),
            }
        }
    }
}

fn hex4(b: &[u8], at: usize) -> Option<u32> {
    let s = std::str::from_utf8(b.get(at..at + 4)?).ok()?;
    u32::from_str_radix(s, 16).ok()
}

/// Parse the single value in `range`.
pub fn parse(b: &[u8], range: (usize, usize)) -> XResult<Node<'_>> {
    let slice = &b[..range.1];
    let mut p = Parser { b: slice, i: range.0, depth: 0 };
    let node = p.value()?;
    p.ws();
    if p.i != range.1 {
        return p.err("unexpected data after value");
    }
    Ok(node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::write_json_compact;

    fn compact(b: &[u8], r: (usize, usize)) -> String {
        let mut out = Vec::new();
        write_json_compact(&mut out, &parse(b, r).unwrap());
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn locate_shapes() {
        let b = br#" [ {"a":1} , [2,3], "x\"]" ,null ] "#;
        let l = locate(b, 0, "").unwrap();
        assert_eq!(l.ranges.len(), 4);
        assert_eq!(compact(b, l.ranges[2]), r#""x\"]""#);

        let b = br#"{"meta":{"n":2},"items":[{"id":"1"},{"id":"2"}],"small":[1]}"#;
        let l = locate(b, 0, "").unwrap();
        assert_eq!(l.path, "items");
        assert_eq!(l.ranges.len(), 2);
        let l = locate(b, 0, "meta.n").unwrap();
        assert_eq!(compact(b, l.ranges[0]), "2");
        assert_eq!(locate(b, 0, "$").unwrap().ranges.len(), 1);
        assert!(locate(b, 0, "nope").is_err());

        let b = b"{\"a\":1}\n{\"a\":2}\n\n{\"a\":3}\n";
        let l = locate(b, 0, "").unwrap();
        assert_eq!(l.path, "lines");
        assert_eq!(l.ranges.len(), 3);

        let b = br#"{"k\"ey":[1,2]}"#;
        assert_eq!(locate(b, 0, "").unwrap().path, "k\"ey");
    }

    #[test]
    fn parse_values() {
        let raw = r#"{"n":-0.5e+10,"s":"aBu00e9Bud83dBude00Bn","t":true,"z":null,"d":1,"d":2,"e":{},"l":[]}"#.replace('B', "\\");
        let b = raw.as_bytes();
        assert_eq!(compact(b, (0, b.len())), "{\"n\":-0.5e+10,\"s\":\"aé😀\\n\",\"t\":true,\"z\":null,\"d\":1,\"d\":2,\"e\":{},\"l\":[]}");
        for bad in [&b"[1,]"[..], b"{\"a\" 1}", b"01", b"\"a\tb\"", b"[1 2]", b"tru", b"1.", b"{\"a\":1,}"] {
            assert!(parse(bad, (0, bad.len())).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }
}
