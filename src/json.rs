//! JSON writers that produce exactly what Python's `json.dumps` produces with
//! `ensure_ascii=False` and either `separators=(",", ":")` or `indent=2`.

use crate::xml::{Obj, Value};

static ESCAPE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 0x20 {
        t[i] = b'u';
        i += 1;
    }
    t[b'"' as usize] = b'"';
    t[b'\\' as usize] = b'\\';
    t[0x08] = b'b';
    t[0x0c] = b'f';
    t[b'\n' as usize] = b'n';
    t[b'\r' as usize] = b'r';
    t[b'\t' as usize] = b't';
    t
};

#[inline]
pub fn write_str(out: &mut Vec<u8>, value: &str) {
    let bytes = value.as_bytes();
    out.reserve(bytes.len() + 2);
    out.push(b'"');
    let mut plain = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let e = ESCAPE[b as usize];
        if e == 0 {
            continue;
        }
        out.extend_from_slice(&bytes[plain..i]);
        if e == b'u' {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            out.extend_from_slice(&[b'\\', b'u', b'0', b'0', HEX[(b >> 4) as usize], HEX[(b & 15) as usize]]);
        } else {
            out.extend_from_slice(&[b'\\', e]);
        }
        plain = i + 1;
    }
    out.extend_from_slice(&bytes[plain..]);
    out.push(b'"');
}

pub fn write_compact(out: &mut Vec<u8>, obj: &Obj) {
    out.push(b'{');
    for (n, (key, value)) in obj.entries.iter().enumerate() {
        if n > 0 {
            out.push(b',');
        }
        write_str(out, key);
        out.push(b':');
        match value {
            Value::Str(v) => write_str(out, v),
            Value::List(items) => {
                out.push(b'[');
                for (m, item) in items.iter().enumerate() {
                    if m > 0 {
                        out.push(b',');
                    }
                    write_compact(out, item);
                }
                out.push(b']');
            }
        }
    }
    out.push(b'}');
}

#[inline]
fn newline(out: &mut Vec<u8>, indent: usize) {
    out.push(b'\n');
    out.resize(out.len() + indent, b' ');
}

/// Pretty-print `obj` whose opening brace sits at nesting `indent` spaces.
pub fn write_pretty(out: &mut Vec<u8>, obj: &Obj, indent: usize) {
    if obj.entries.is_empty() {
        out.extend_from_slice(b"{}");
        return;
    }
    out.push(b'{');
    let inner = indent + 2;
    for (n, (key, value)) in obj.entries.iter().enumerate() {
        if n > 0 {
            out.push(b',');
        }
        newline(out, inner);
        write_str(out, key);
        out.extend_from_slice(b": ");
        match value {
            Value::Str(v) => write_str(out, v),
            Value::List(items) if items.is_empty() => out.extend_from_slice(b"[]"),
            Value::List(items) => {
                out.push(b'[');
                for (m, item) in items.iter().enumerate() {
                    if m > 0 {
                        out.push(b',');
                    }
                    newline(out, inner + 2);
                    write_pretty(out, item, inner + 2);
                }
                newline(out, inner);
                out.push(b']');
            }
        }
    }
    newline(out, indent);
    out.push(b'}');
}
