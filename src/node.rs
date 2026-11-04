//! A format-neutral record model, the clean-up options, and the writers for
//! every output format.
//!
//! XML records arrive as [`crate::xml::Obj`] (the mapping that must match the
//! Python reference) and are turned into a [`Node`] only when an option or an
//! output format needs it; JSON records are parsed straight into a `Node`.

use std::borrow::Cow;

use crate::json::write_str;
use crate::xml::{Obj, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Node<'a> {
    Null,
    Bool(bool),
    /// A number exactly as written in the input.
    Num(Cow<'a, str>),
    Str(Cow<'a, str>),
    Arr(Vec<Node<'a>>),
    Obj(Vec<(Cow<'a, str>, Node<'a>)>),
}

impl<'a> Node<'a> {
    pub fn is_scalar(&self) -> bool {
        !matches!(self, Node::Arr(_) | Node::Obj(_))
    }

    /// The value as plain text for CSV cells and summaries.
    pub fn scalar_text(&self) -> Option<Cow<'_, str>> {
        match self {
            Node::Null => Some(Cow::Borrowed("")),
            Node::Bool(b) => Some(Cow::Borrowed(if *b { "true" } else { "false" })),
            Node::Num(n) => Some(Cow::Borrowed(n)),
            Node::Str(s) => Some(Cow::Borrowed(s)),
            _ => None,
        }
    }
}

/// Convert an XML record. Attributes get `attr_prefix` in front of their name.
pub fn from_obj<'a>(obj: Obj<'a>, attr_prefix: &str) -> Node<'a> {
    let mut entries = Vec::with_capacity(obj.entries.len());
    for (key, value) in obj.entries {
        match value {
            Value::Str(s) => {
                let key = if key == "#text" || attr_prefix.is_empty() { Cow::Borrowed(key) } else { Cow::Owned(format!("{attr_prefix}{key}")) };
                entries.push((key, Node::Str(s)));
            }
            Value::List(items) => {
                entries.push((Cow::Borrowed(key), Node::Arr(items.into_iter().map(|o| from_obj(o, attr_prefix)).collect())));
            }
        }
    }
    Node::Obj(entries)
}

// ---------------------------------------------------------------------------
// Clean-up options
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Options {
    /// Trim leading and trailing whitespace from every string.
    pub trim: bool,
    /// Turn every run of whitespace inside a string into one space.
    pub collapse_spaces: bool,
    /// Remove empty strings, nulls, `{}` and `[]`, bottom-up.
    pub remove_empty: bool,
    /// Replace a one-item list by its item.
    pub unwrap_single: bool,
    /// Replace `{"#text": v}` (an object with only text) by `v`.
    pub text_values: bool,
    /// Prefix for XML attribute names, e.g. "@" (xmltodict style).
    pub attr_prefix: String,
    /// Remove these keys wherever they occur.
    pub drop_keys: Vec<String>,
}

impl Options {
    /// No option changes anything: the output equals the plain mapping.
    pub fn is_plain(&self) -> bool {
        *self == Options::default()
    }

    /// xmltodict-like: `@` attributes, plain text values, no one-item lists.
    pub fn simple() -> Options {
        Options { unwrap_single: true, text_values: true, attr_prefix: "@".into(), ..Options::default() }
    }

    /// Parse "a, b ,c" into keys to drop.
    pub fn set_drop_keys(&mut self, list: &str) {
        self.drop_keys = list.split(',').map(str::trim).filter(|k| !k.is_empty()).map(String::from).collect();
    }

    /// Apply to a whole record. The record itself is never removed: if
    /// everything in it is empty it stays `{}`, so record counts never change.
    pub fn apply<'a>(&self, node: Node<'a>) -> Node<'a> {
        if self.is_plain() {
            return node;
        }
        let root_is_obj = matches!(node, Node::Obj(_));
        match self.clean(node) {
            Some(n) => n,
            None if root_is_obj => Node::Obj(Vec::new()),
            None => Node::Null,
        }
    }

    fn clean<'a>(&self, node: Node<'a>) -> Option<Node<'a>> {
        match node {
            Node::Str(s) => {
                let s = self.clean_str(s);
                if self.remove_empty && s.is_empty() { None } else { Some(Node::Str(s)) }
            }
            Node::Null if self.remove_empty => None,
            Node::Arr(items) => {
                let mut items: Vec<Node<'a>> = items.into_iter().filter_map(|n| self.clean(n)).collect();
                if self.remove_empty && items.is_empty() {
                    return None;
                }
                if self.unwrap_single && items.len() == 1 {
                    return items.pop();
                }
                Some(Node::Arr(items))
            }
            Node::Obj(entries) => {
                let mut out = Vec::with_capacity(entries.len());
                for (key, value) in entries {
                    if self.drop_keys.iter().any(|d| d.as_str() == key.as_ref()) {
                        continue;
                    }
                    if let Some(v) = self.clean(value) {
                        out.push((key, v));
                    }
                }
                if self.remove_empty && out.is_empty() {
                    return None;
                }
                if self.text_values && out.len() == 1 && out[0].0 == "#text" && out[0].1.is_scalar() {
                    return out.pop().map(|(_, v)| v);
                }
                Some(Node::Obj(out))
            }
            other => Some(other),
        }
    }

    fn clean_str<'a>(&self, s: Cow<'a, str>) -> Cow<'a, str> {
        let mut s = s;
        if self.collapse_spaces {
            if let Some(c) = collapse(&s) {
                s = Cow::Owned(c);
            }
        }
        if self.trim {
            s = match s {
                Cow::Borrowed(b) => Cow::Borrowed(b.trim()),
                Cow::Owned(o) => {
                    let t = o.trim();
                    if t.len() == o.len() { Cow::Owned(o) } else { Cow::Owned(t.to_string()) }
                }
            };
        }
        s
    }
}

/// `Some(collapsed)` when `s` has a whitespace run longer than one plain
/// space (or any tab, newline…); `None` when nothing would change.
fn collapse(s: &str) -> Option<String> {
    let mut prev_ws = false;
    let needed = s.chars().any(|c| {
        let ws = c.is_whitespace();
        let bad = ws && (prev_ws || c != ' ');
        prev_ws = ws;
        bad
    });
    if !needed {
        return None;
    }
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// Same bytes as `json::write_compact` for XML-derived nodes.
pub fn write_json_compact(out: &mut Vec<u8>, node: &Node) {
    match node {
        Node::Null => out.extend_from_slice(b"null"),
        Node::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Node::Num(n) => out.extend_from_slice(n.as_bytes()),
        Node::Str(s) => write_str(out, s),
        Node::Arr(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_json_compact(out, item);
            }
            out.push(b']');
        }
        Node::Obj(entries) => {
            out.push(b'{');
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_str(out, k);
                out.push(b':');
                write_json_compact(out, v);
            }
            out.push(b'}');
        }
    }
}

fn newline(out: &mut Vec<u8>, indent: usize) {
    out.push(b'\n');
    out.resize(out.len() + indent, b' ');
}

/// Same bytes as Python's `json.dumps(indent=2)` for a value whose first
/// character sits at nesting `indent`.
pub fn write_json_pretty(out: &mut Vec<u8>, node: &Node, indent: usize) {
    match node {
        Node::Arr(items) if items.is_empty() => out.extend_from_slice(b"[]"),
        Node::Obj(entries) if entries.is_empty() => out.extend_from_slice(b"{}"),
        Node::Arr(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                newline(out, indent + 2);
                write_json_pretty(out, item, indent + 2);
            }
            newline(out, indent);
            out.push(b']');
        }
        Node::Obj(entries) => {
            out.push(b'{');
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                newline(out, indent + 2);
                write_str(out, k);
                out.extend_from_slice(b": ");
                write_json_pretty(out, v, indent + 2);
            }
            newline(out, indent);
            out.push(b'}');
        }
        scalar => write_json_compact(out, scalar),
    }
}

// ---------------------------------------------------------------------------
// YAML
// ---------------------------------------------------------------------------

/// Keys are written bare when that cannot be misread, quoted otherwise.
/// String values are always double-quoted, so "00123", "yes" or "null" stay
/// strings in every YAML reader (1.1 and 1.2).
fn yaml_key(out: &mut Vec<u8>, key: &str) {
    let plain = !key.is_empty()
        && key.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        && !matches!(
            key.to_ascii_lowercase().as_str(),
            "y" | "n" | "yes" | "no" | "on" | "off" | "true" | "false" | "null" | "nan" | "inf"
        );
    if plain {
        out.extend_from_slice(key.as_bytes());
    } else {
        write_yaml_str(out, key);
    }
}

/// A double-quoted YAML string. Like JSON, plus escapes for characters YAML
/// does not allow raw (DEL, C1 controls, U+FFFE/U+FFFF) or would read as a
/// line break (NEL, U+2028, U+2029).
fn write_yaml_str(out: &mut Vec<u8>, s: &str) {
    let printable = |c: char| match c as u32 {
        // NEL, LS and PS are line breaks to YAML and would be folded.
        0x7F..=0x9F | 0x2028 | 0x2029 | 0xFFFE | 0xFFFF => false,
        _ => true,
    };
    if s.chars().all(printable) {
        write_str(out, s);
        return;
    }
    let mut piece = String::new();
    out.push(b'"');
    for c in s.chars() {
        if printable(c) {
            piece.push(c);
            continue;
        }
        let mut quoted = Vec::new();
        write_str(&mut quoted, &piece);
        out.extend_from_slice(&quoted[1..quoted.len() - 1]);
        piece.clear();
        let code = c as u32;
        let escape = if code < 0x100 { format!("\\x{code:02X}") } else { format!("\\u{code:04X}") };
        out.extend_from_slice(escape.as_bytes());
    }
    let mut quoted = Vec::new();
    write_str(&mut quoted, &piece);
    out.extend_from_slice(&quoted[1..quoted.len() - 1]);
    out.push(b'"');
}

fn yaml_scalar(out: &mut Vec<u8>, node: &Node) {
    match node {
        Node::Null => out.extend_from_slice(b"null"),
        Node::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        // JSON numbers are valid YAML numbers, except for some forms YAML 1.1
        // reads differently; quoting those is not needed for JSON's syntax.
        Node::Num(n) => out.extend_from_slice(n.as_bytes()),
        Node::Str(s) => write_yaml_str(out, s),
        Node::Arr(_) => out.extend_from_slice(b"[]"),
        Node::Obj(_) => out.extend_from_slice(b"{}"),
    }
}

fn is_empty_container(node: &Node) -> bool {
    matches!(node, Node::Arr(a) if a.is_empty()) || matches!(node, Node::Obj(o) if o.is_empty())
}

/// Write `node` as the value after "key:" or "- " at nesting `indent`.
/// `inline` is true when the cursor is right after "- " (a list item).
fn write_yaml_value(out: &mut Vec<u8>, node: &Node, indent: usize, inline: bool) {
    if node.is_scalar() || is_empty_container(node) {
        if !inline {
            out.push(b' ');
        }
        yaml_scalar(out, node);
        return;
    }
    match node {
        Node::Arr(items) => {
            for (i, item) in items.iter().enumerate() {
                if i > 0 || !inline {
                    newline(out, indent);
                }
                out.extend_from_slice(b"- ");
                write_yaml_value(out, item, indent + 2, true);
            }
        }
        Node::Obj(entries) => {
            for (i, (k, v)) in entries.iter().enumerate() {
                if i > 0 || !inline {
                    newline(out, indent);
                }
                yaml_key(out, k);
                out.push(b':');
                let child_indent = if matches!(v, Node::Arr(_)) { indent } else { indent + 2 };
                write_yaml_value(out, v, child_indent, false);
            }
        }
        _ => unreachable!(),
    }
}

/// One record as an item of the top-level YAML sequence.
pub fn write_yaml_record(out: &mut Vec<u8>, node: &Node) {
    out.extend_from_slice(b"- ");
    write_yaml_value(out, node, 2, true);
    out.push(b'\n');
}

// ---------------------------------------------------------------------------
// XML
// ---------------------------------------------------------------------------

/// How scalars inside objects are written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XmlScalars {
    /// `key="value"` (round-trips XML read with the default mapping).
    Attributes,
    /// `<key>value</key>`.
    Elements,
}

pub struct XmlStyle<'s> {
    pub scalars: XmlScalars,
    /// Keys starting with this are always attributes (prefix removed).
    pub attr_prefix: &'s str,
}

/// A valid XML name made from any key: invalid characters become `_`, and
/// a name that cannot start a tag gets a leading `_`.
pub fn xml_name(key: &str) -> Cow<'_, str> {
    let ok_start = |c: char| c.is_alphabetic() || c == '_';
    let ok = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '.');
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if ok_start(c) && chars.all(ok) && !key.to_ascii_lowercase().starts_with("xml") => Cow::Borrowed(key),
        _ => {
            let mut out = String::with_capacity(key.len() + 1);
            let body: String = key.chars().map(|c| if ok(c) { c } else { '_' }).collect();
            if !body.chars().next().is_some_and(ok_start) || body.to_ascii_lowercase().starts_with("xml") {
                out.push('_');
            }
            out.push_str(&body);
            if out.is_empty() {
                out.push('_');
            }
            Cow::Owned(out)
        }
    }
}

pub fn xml_escape(out: &mut Vec<u8>, s: &str, attribute: bool) {
    let mut plain = 0;
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        let rep: &[u8] = match b {
            b'&' => b"&amp;",
            b'<' => b"&lt;",
            b'>' => b"&gt;",
            b'"' if attribute => b"&quot;",
            b'\n' if attribute => b"&#10;",
            b'\t' if attribute => b"&#9;",
            b'\r' => b"&#13;",
            _ => continue,
        };
        out.extend_from_slice(&bytes[plain..i]);
        out.extend_from_slice(rep);
        plain = i + 1;
    }
    out.extend_from_slice(&bytes[plain..]);
}

/// Write `node` as element `name` at `indent` (no leading newline).
pub fn write_xml_element(out: &mut Vec<u8>, name: &str, node: &Node, indent: usize, style: &XmlStyle) {
    let name = xml_name(name);
    out.push(b'<');
    out.extend_from_slice(name.as_bytes());
    match node {
        Node::Null => out.extend_from_slice(b"/>"),
        Node::Bool(_) | Node::Num(_) | Node::Str(_) => {
            out.push(b'>');
            xml_escape(out, &node.scalar_text().unwrap(), false);
            close_tag(out, &name);
        }
        Node::Arr(items) => {
            // A list that is not under a key: wrap its items in <item>.
            if items.is_empty() {
                out.extend_from_slice(b"/>");
                return;
            }
            out.push(b'>');
            for item in items {
                newline(out, indent + 2);
                write_xml_element(out, "item", item, indent + 2, style);
            }
            newline(out, indent);
            close_tag(out, &name);
        }
        Node::Obj(entries) => {
            let mut text: Option<Cow<str>> = None;
            let mut children: Vec<(&str, &Node)> = Vec::new();
            for (k, v) in entries {
                let as_attr_key = if !style.attr_prefix.is_empty() { k.strip_prefix(style.attr_prefix) } else { None };
                if k == "#text" && v.is_scalar() {
                    text = v.scalar_text();
                } else if let Some(attr) = as_attr_key.filter(|_| v.is_scalar()) {
                    write_attr(out, attr, v);
                } else if v.is_scalar() && style.scalars == XmlScalars::Attributes && style.attr_prefix.is_empty() {
                    write_attr(out, k, v);
                } else {
                    children.push((k, v));
                }
            }
            if text.is_none() && children.is_empty() {
                out.extend_from_slice(b"/>");
                return;
            }
            out.push(b'>');
            if let Some(t) = &text {
                xml_escape(out, t, false);
            }
            if !children.is_empty() {
                for (k, v) in children {
                    match v {
                        // A list under a key repeats the element.
                        Node::Arr(items) => {
                            for item in items {
                                newline(out, indent + 2);
                                write_xml_element(out, k, item, indent + 2, style);
                            }
                        }
                        _ => {
                            newline(out, indent + 2);
                            write_xml_element(out, k, v, indent + 2, style);
                        }
                    }
                }
                newline(out, indent);
            }
            close_tag(out, &name);
        }
    }
}

fn write_attr(out: &mut Vec<u8>, key: &str, value: &Node) {
    if matches!(value, Node::Null) {
        return;
    }
    out.push(b' ');
    out.extend_from_slice(xml_name(key).as_bytes());
    out.extend_from_slice(b"=\"");
    xml_escape(out, &value.scalar_text().unwrap(), true);
    out.push(b'"');
}

fn close_tag(out: &mut Vec<u8>, name: &str) {
    out.extend_from_slice(b"</");
    out.extend_from_slice(name.as_bytes());
    out.push(b'>');
}

// ---------------------------------------------------------------------------
// CSV / TSV
// ---------------------------------------------------------------------------

/// Flatten a record into (column, value) pairs.
///
/// Nested keys are joined with "."; `#text` adds nothing to the path, so
/// `name: [{"#text": "Bolt"}]` becomes the column `name`. Lists add nothing
/// either: every item contributes to the same columns, and a column that
/// gets several values holds them joined with "; ".
pub fn flatten(node: &Node, out: &mut Vec<(String, String)>) {
    fn walk(node: &Node, path: &mut String, out: &mut Vec<(String, String)>) {
        match node {
            Node::Obj(entries) => {
                for (k, v) in entries {
                    let len = path.len();
                    if k != "#text" {
                        if !path.is_empty() {
                            path.push('.');
                        }
                        path.push_str(k);
                    } else if path.is_empty() {
                        path.push_str("#text");
                    }
                    walk(v, path, out);
                    path.truncate(len);
                }
            }
            Node::Arr(items) => {
                for item in items {
                    walk(item, path, out);
                }
            }
            scalar => {
                let text = scalar.scalar_text().unwrap();
                let column = if path.is_empty() { "value" } else { path.as_str() };
                match out.iter_mut().find(|(c, _)| c == column) {
                    Some((_, v)) => {
                        v.push_str("; ");
                        v.push_str(&text);
                    }
                    None => out.push((column.to_string(), text.into_owned())),
                }
            }
        }
    }
    walk(node, &mut String::new(), out);
}

/// One CSV field, quoted when needed (RFC 4180).
pub fn write_csv_field(out: &mut Vec<u8>, value: &str, delimiter: u8) {
    let needs = value.bytes().any(|b| b == delimiter || matches!(b, b'"' | b'\n' | b'\r'))
        || value.starts_with(' ')
        || value.ends_with(' ');
    if !needs {
        out.extend_from_slice(value.as_bytes());
        return;
    }
    out.push(b'"');
    for b in value.bytes() {
        if b == b'"' {
            out.push(b'"');
        }
        out.push(b);
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Node<'_> {
        Node::Str(Cow::Borrowed(v))
    }
    fn obj<'a>(e: Vec<(&'a str, Node<'a>)>) -> Node<'a> {
        Node::Obj(e.into_iter().map(|(k, v)| (Cow::Borrowed(k), v)).collect())
    }
    fn json(n: &Node) -> String {
        let mut out = Vec::new();
        write_json_compact(&mut out, n);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn clean_up() {
        let rec = obj(vec![
            ("id", s("  007 ")),
            ("note", s("")),
            ("name", Node::Arr(vec![obj(vec![("#text", s("Bolt \n  M8"))])])),
            ("empty", Node::Arr(vec![obj(vec![]), obj(vec![("x", s(" "))])])),
            ("secret", s("x")),
        ]);
        let mut o = Options { trim: true, remove_empty: true, ..Default::default() };
        o.set_drop_keys(" secret , ");
        assert_eq!(json(&o.apply(rec.clone())), r##"{"id":"007","name":[{"#text":"Bolt \n  M8"}]}"##);
        let o = Options { collapse_spaces: true, unwrap_single: true, text_values: true, ..Default::default() };
        assert_eq!(json(&o.apply(rec.clone())), r##"{"id":" 007 ","note":"","name":"Bolt M8","empty":[{},{"x":" "}],"secret":"x"}"##);
        // An empty record stays an object.
        let o = Options { remove_empty: true, ..Default::default() };
        assert_eq!(json(&o.apply(obj(vec![("a", s(""))]))), "{}");
        assert!(Options::default().is_plain());
    }

    #[test]
    fn yaml_quotes_everything_risky() {
        let rec = obj(vec![("id", s("00123")), ("yes", s("no")), ("list", Node::Arr(vec![s("a"), obj(vec![("k", Node::Num("1.5".into()))])])), ("e", Node::Arr(vec![]))]);
        let mut out = Vec::new();
        write_yaml_record(&mut out, &rec);
        assert_eq!(String::from_utf8(out).unwrap(), "- id: \"00123\"\n  \"yes\": \"no\"\n  list:\n  - \"a\"\n  - k: 1.5\n  e: []\n");
    }

    #[test]
    fn xml_round_trip_shape() {
        let rec = obj(vec![("id", s("1")), ("#text", s("a<b")), ("name", Node::Arr(vec![obj(vec![("#text", s("Bolt"))]), obj(vec![])]))]);
        let mut out = Vec::new();
        write_xml_element(&mut out, "part", &rec, 0, &XmlStyle { scalars: XmlScalars::Attributes, attr_prefix: "" });
        assert_eq!(String::from_utf8(out).unwrap(), "<part id=\"1\">a&lt;b\n  <name>Bolt</name>\n  <name/>\n</part>");
        assert_eq!(xml_name("1 bad key"), "_1_bad_key");
        assert_eq!(xml_name("xmlish"), "_xmlish");
    }

    #[test]
    fn csv_flattening() {
        let rec = obj(vec![
            ("id", s("7")),
            ("name", Node::Arr(vec![obj(vec![("#text", s("Bolt"))])])),
            ("price", Node::Arr(vec![obj(vec![("cur", s("EUR")), ("#text", s("1.5"))]), obj(vec![("cur", s("USD")), ("#text", s("2"))])])),
        ]);
        let mut cols = Vec::new();
        flatten(&rec, &mut cols);
        assert_eq!(
            cols,
            vec![("id".into(), "7".into()), ("name".into(), "Bolt".into()), ("price.cur".into(), "EUR; USD".into()), ("price".into(), "1.5; 2".into())]
        );
        let mut out = Vec::new();
        write_csv_field(&mut out, "a \"q\", b", b',');
        assert_eq!(out, b"\"a \"\"q\"\", b\"");
    }
}
