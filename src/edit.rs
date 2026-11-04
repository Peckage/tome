//! Editing huge files: a piece table over lines.
//!
//! The document is a list of pieces. An `Orig` piece is a run of unchanged
//! lines of the memory-mapped file; a `New` piece holds lines typed or
//! pasted. Opening a 500 MB file and changing one line costs three pieces,
//! not a copy of the file. Snapshots for undo are cheap clones of the piece
//! list (the text itself is shared).
//!
//! Saving streams the pieces to a temporary file: unchanged runs are copied
//! byte for byte (their line endings included), new lines get the file's
//! line ending, and a file without a final newline keeps not having one.

use std::borrow::Cow;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::engine::Progress;
use crate::text::{self, Pattern, TextFile};

#[derive(Clone, Debug, PartialEq)]
enum Piece {
    /// Lines `first..first + count` of the original file.
    Orig { first: usize, count: usize },
    New(Arc<Vec<String>>),
}

impl Piece {
    fn len(&self) -> usize {
        match self {
            Piece::Orig { count, .. } => *count,
            Piece::New(lines) => lines.len(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Edits {
    pieces: Vec<Piece>,
    /// First view line of each piece.
    starts: Vec<usize>,
    total: usize,
    original_lines: usize,
}

impl Edits {
    pub fn new(original_lines: usize) -> Edits {
        let pieces = if original_lines > 0 { vec![Piece::Orig { first: 0, count: original_lines }] } else { Vec::new() };
        let mut e = Edits { pieces, starts: Vec::new(), total: 0, original_lines };
        e.reindex();
        e
    }

    fn reindex(&mut self) {
        self.starts.clear();
        let mut at = 0;
        for p in &self.pieces {
            self.starts.push(at);
            at += p.len();
        }
        self.total = at;
    }

    /// Nothing changed: the document is the original file.
    pub fn is_plain(&self) -> bool {
        *self == Edits::new(self.original_lines)
    }

    pub fn line_count(&self) -> usize {
        self.total
    }

    /// (piece, offset in piece) of view line `line` (< total).
    fn locate(&self, line: usize) -> (usize, usize) {
        let p = self.starts.partition_point(|&s| s <= line) - 1;
        (p, line - self.starts[p])
    }

    /// Text of view line `line`, without its line ending.
    pub fn line<'a>(&'a self, file: &'a TextFile, line: usize) -> Cow<'a, [u8]> {
        let (p, off) = self.locate(line);
        match &self.pieces[p] {
            Piece::Orig { first, .. } => Cow::Borrowed(file.line(first + off)),
            Piece::New(lines) => Cow::Borrowed(lines[off].as_bytes()),
        }
    }

    /// Was this line typed or pasted (not from the file on disk)?
    pub fn is_new(&self, line: usize) -> bool {
        line < self.total && matches!(self.pieces[self.locate(line).0], Piece::New(_))
    }

    /// Where original line `orig` is now, if it is still there unchanged.
    /// Orig pieces stay in file order, so this is a binary search.
    pub fn view_of_original(&self, orig: usize) -> Option<usize> {
        let mut lo = 0usize;
        let mut hi = self.pieces.len();
        // Binary search over pieces, skipping New ones by scanning outwards.
        while lo < hi {
            let mid = (lo + hi) / 2;
            let mut m = mid;
            while m < hi && matches!(self.pieces[m], Piece::New(_)) {
                m += 1;
            }
            if m == hi {
                hi = mid;
                continue;
            }
            let Piece::Orig { first, count } = self.pieces[m] else { unreachable!() };
            if orig < first {
                hi = mid;
            } else if orig >= first + count {
                lo = m + 1;
            } else {
                return Some(self.starts[m] + orig - first);
            }
        }
        None
    }

    /// Split so that a piece starts at view line `line`; returns its index.
    fn split(&mut self, line: usize) -> usize {
        if line >= self.total {
            return self.pieces.len();
        }
        let (p, off) = self.locate(line);
        if off == 0 {
            return p;
        }
        let (a, b) = match &self.pieces[p] {
            Piece::Orig { first, count } => (Piece::Orig { first: *first, count: off }, Piece::Orig { first: first + off, count: count - off }),
            Piece::New(lines) => (Piece::New(Arc::new(lines[..off].to_vec())), Piece::New(Arc::new(lines[off..].to_vec()))),
        };
        self.pieces[p] = a;
        self.pieces.insert(p + 1, b);
        self.reindex();
        p + 1
    }

    /// Replace view lines `first..last` with `lines` (an insert when
    /// `first == last`, a delete when `lines` is empty).
    pub fn splice(&mut self, first: usize, last: usize, lines: Vec<String>) {
        let first = first.min(self.total);
        let last = last.clamp(first, self.total);
        let a = self.split(first);
        let b = self.split(last);
        let mut replacement = Vec::new();
        if !lines.is_empty() {
            replacement.push(Piece::New(Arc::new(lines)));
        }
        self.pieces.splice(a..b, replacement);
        self.merge_new(a);
        self.reindex();
    }

    /// Join neighbouring pieces around `at`: New with New (so typing line
    /// after line does not grow the list without bound) and Orig runs that
    /// continue each other (so an edit that changes nothing is "plain").
    fn merge_new(&mut self, at: usize) {
        let mut i = at.saturating_sub(1);
        while i + 1 < self.pieces.len() && i <= at + 1 {
            let merged = match (&self.pieces[i], &self.pieces[i + 1]) {
                (Piece::New(a), Piece::New(b)) => {
                    let mut joined = a.as_ref().clone();
                    joined.extend(b.iter().cloned());
                    Some(Piece::New(Arc::new(joined)))
                }
                (Piece::Orig { first: a, count: n }, Piece::Orig { first: b, count: m }) if a + n == *b => Some(Piece::Orig { first: *a, count: n + m }),
                _ => None,
            };
            match merged {
                Some(p) => {
                    self.pieces[i] = p;
                    self.pieces.remove(i + 1);
                }
                None => i += 1,
            }
        }
    }

    pub fn set_line(&mut self, line: usize, text: String) {
        self.splice(line, line + 1, vec![text]);
    }

    /// Lines `first..=last` joined with `\n`.
    pub fn copy_lines(&self, file: &TextFile, first: usize, last: usize, limit: usize) -> (String, bool) {
        let mut out = Vec::new();
        let mut cut = false;
        for i in first..=last.min(self.total.saturating_sub(1)) {
            if i > first {
                out.push(b'\n');
            }
            out.extend_from_slice(&self.line(file, i));
            if out.len() > limit {
                out.truncate(limit);
                cut = true;
                break;
            }
        }
        (String::from_utf8_lossy(&out).into_owned(), cut)
    }

    /// Write the document. See the module docs for what is kept as is.
    pub fn write_to(&self, file: &TextFile, out: &mut impl Write) -> io::Result<()> {
        let bytes = file.bytes();
        let eol: &[u8] = if file.crlf { b"\r\n" } else { b"\n" };
        let trailing = bytes.is_empty() || bytes.last() == Some(&b'\n');
        let mut pending: Vec<u8> = Vec::new();
        for piece in &self.pieces {
            match piece {
                Piece::Orig { first, count } => {
                    let start = file.line_start(*first) as usize;
                    let end = if first + count < file.line_count() { file.line_start(first + count) as usize } else { bytes.len() };
                    let mut chunk = &bytes[start..end];
                    let mut ending: &[u8] = eol;
                    if chunk.ends_with(b"\r\n") {
                        ending = b"\r\n";
                        chunk = &chunk[..chunk.len() - 2];
                    } else if chunk.ends_with(b"\n") {
                        ending = b"\n";
                        chunk = &chunk[..chunk.len() - 1];
                    }
                    out.write_all(&pending)?;
                    out.write_all(chunk)?;
                    pending = ending.to_vec();
                }
                Piece::New(lines) => {
                    for line in lines.iter() {
                        out.write_all(&pending)?;
                        out.write_all(line.as_bytes())?;
                        pending = eol.to_vec();
                    }
                }
            }
        }
        if trailing {
            out.write_all(&pending)?;
        }
        Ok(())
    }
}

/// Saving is two steps. [`write_temporary`] writes the document next to
/// `target` while the original is still mapped (it is read from). Then the
/// caller drops its mappings and [`finish_save`] renames the temporary over
/// the target. The original is never written in place: truncating a mapped
/// file could crash the program, and Windows cannot replace a mapped file.
pub fn write_temporary(edits: &Edits, file: &TextFile, target: &Path) -> io::Result<PathBuf> {
    let mut temporary = target.as_os_str().to_owned();
    temporary.push(".tome-save");
    let temporary = PathBuf::from(temporary);
    let result = (|| {
        let f = std::fs::File::create(&temporary)?;
        let mut w = io::BufWriter::with_capacity(1 << 20, f);
        edits.write_to(file, &mut w)?;
        let f = w.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()
    })();
    match result {
        Ok(()) => Ok(temporary),
        Err(e) => {
            let _ = std::fs::remove_file(&temporary);
            Err(e)
        }
    }
}

/// Move the temporary over `target`, with the permissions of `source`. On
/// failure the temporary is kept, so the edits are not lost.
pub fn finish_save(temporary: &Path, source: &Path, target: &Path) -> io::Result<()> {
    if let Ok(meta) = std::fs::metadata(source) {
        let _ = std::fs::set_permissions(temporary, meta.permissions());
    }
    crate::engine::replace_file(temporary, target)
}

/// A search hit on a (possibly edited) line: byte columns within the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hit {
    pub line: usize,
    pub start: u32,
    pub end: u32,
}

/// Search the document as edited. The mapped file is searched on all
/// cores as before; hits on lines that are gone are dropped, the rest move
/// to where their line is now, and typed lines are searched separately.
pub fn find(file: &TextFile, edits: &Edits, pattern: &Pattern, limit: usize, progress: &Progress) -> crate::engine::Result<(Vec<Hit>, bool)> {
    let (raw, truncated) = text::find_all(file, pattern, limit, progress)?;
    let plain = edits.is_plain();
    let mut hits = Vec::with_capacity(raw.len());
    for (s, e) in raw {
        let orig = file.line_of(s);
        let (ls, le) = file.line_range(orig);
        let start = (s as usize - ls) as u32;
        let end = (e as usize).min(le).saturating_sub(ls).max(start as usize + 1) as u32;
        let line = if plain { Some(orig) } else { edits.view_of_original(orig) };
        if let Some(line) = line {
            hits.push(Hit { line, start, end });
        }
    }
    if !plain {
        for (p, piece) in edits.pieces.iter().enumerate() {
            if let Piece::New(lines) = piece {
                for (k, text) in lines.iter().enumerate() {
                    pattern.for_each(text.as_bytes(), |s, e| {
                        hits.push(Hit { line: edits.starts[p] + k, start: s as u32, end: e as u32 });
                        hits.len() < limit
                    });
                }
            }
        }
        hits.sort_unstable();
        hits.truncate(limit);
    }
    let truncated = truncated || hits.len() >= limit;
    Ok((hits, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, content: &[u8]) -> (TextFile, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("tome-edit-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        (TextFile::open(&path).unwrap(), path)
    }

    fn written(e: &Edits, f: &TextFile) -> Vec<u8> {
        let mut out = Vec::new();
        e.write_to(f, &mut out).unwrap();
        out
    }

    /// Saving while a background job still has the old file mapped: Windows
    /// refuses to replace a mapped file, so the save waits for the job to let
    /// go instead of failing.
    #[test]
    fn save_over_a_file_still_mapped_elsewhere() {
        let (f, path) = file("mapped.txt", b"old\n");
        let mut e = Edits::new(f.line_count());
        e.set_line(0, "new".into());
        let temporary = write_temporary(&e, &f, &path).unwrap();
        let held = std::sync::Arc::new(f);
        let job = {
            let held = held.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                drop(held);
            })
        };
        drop(held);
        finish_save(&temporary, &path, &path).unwrap();
        job.join().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n");
    }

    #[test]
    fn unedited_save_is_identical() {
        for (name, content) in [
            ("lf.txt", &b"a\nb\nc\n"[..]),
            ("crlf.txt", b"a\r\nb\r\n"),
            ("nofinal.txt", b"a\nb"),
            ("empty.txt", b""),
            ("blank.txt", b"\n\n"),
            ("bad-utf8.txt", b"ok\n\xff\xfe bad\nend"),
            ("bom.txt", b"\xef\xbb\xbfbom\nline\n"),
            ("mixed.txt", b"a\r\nb\nc\r\n"),
        ] {
            let (f, _) = file(name, content);
            let e = Edits::new(f.line_count());
            assert!(e.is_plain());
            assert_eq!(written(&e, &f), content, "{name}");
        }
    }

    #[test]
    fn edits_keep_endings_and_final_newline() {
        let (f, _) = file("e1.txt", b"one\r\ntwo\r\nthree");
        let mut e = Edits::new(f.line_count());
        e.set_line(1, "TWO".into());
        e.splice(3, 3, vec!["four".into()]);
        assert_eq!(written(&e, &f), b"one\r\nTWO\r\nthree\r\nfour");
        e.splice(0, 1, vec![]);
        assert_eq!(e.line(&f, 0).as_ref(), b"TWO");
        let mut plain = Edits::new(f.line_count());
        plain.splice(1, 1, vec![]);
        assert!(plain.is_plain(), "an empty insert changes nothing");
        assert_eq!(e.view_of_original(2), Some(1));
        assert_eq!(e.view_of_original(0), None);
        assert_eq!(e.view_of_original(1), None, "line 1 was replaced");
        assert!(e.is_new(0));
        assert!(!e.is_new(1));
    }

    /// Random edits against a plain list of lines.
    #[test]
    fn random_model() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rand = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n.max(1) as u64) as usize
        };
        for round in 0..200 {
            let lines = rand(40);
            let crlf = round % 3 == 0;
            let final_newline = round % 2 == 0;
            let eol = if crlf { "\r\n" } else { "\n" };
            let mut content = (0..lines).map(|i| format!("line {i}")).collect::<Vec<_>>().join(eol);
            if final_newline && lines > 0 {
                content.push_str(eol);
            }
            let (f, _) = file(&format!("r{round}.txt"), content.as_bytes());
            let mut model: Vec<String> = (0..f.line_count()).map(|i| String::from_utf8(f.line(i).to_vec()).unwrap()).collect();
            let mut e = Edits::new(f.line_count());
            let mut history = vec![(e.clone(), model.clone())];
            for step in 0..60 {
                let n = model.len();
                match rand(6) {
                    0 if n > 0 => {
                        let i = rand(n);
                        let t = format!("edit {round}.{step}");
                        e.set_line(i, t.clone());
                        model[i] = t;
                    }
                    1 => {
                        let i = rand(n + 1);
                        let new: Vec<String> = (0..rand(3) + 1).map(|k| format!("ins {step}.{k}")).collect();
                        e.splice(i, i, new.clone());
                        model.splice(i..i, new);
                    }
                    2 if n > 0 => {
                        let a = rand(n);
                        let b = (a + rand(4)).min(n);
                        e.splice(a, b, vec![]);
                        model.drain(a..b);
                    }
                    3 if n > 1 => {
                        // join two lines (Backspace at the start of a line)
                        let i = rand(n - 1) + 1;
                        let joined = format!("{}{}", model[i - 1], model[i]);
                        e.splice(i - 1, i + 1, vec![joined.clone()]);
                        model.splice(i - 1..i + 1, [joined]);
                    }
                    4 if history.len() > 1 && rand(3) == 0 => {
                        history.pop();
                        let (pe, pm) = history.last().unwrap().clone();
                        e = pe;
                        model = pm;
                        continue;
                    }
                    _ => {}
                }
                history.push((e.clone(), model.clone()));
                assert_eq!(e.line_count(), model.len());
                for (i, m) in model.iter().enumerate() {
                    assert_eq!(e.line(&f, i).as_ref(), m.as_bytes(), "round {round} step {step} line {i}");
                }
            }
            // A file without any line break has no style to keep: LF.
            let eol = if content.contains('\n') { eol } else { "\n" };
            let mut expected = model.join(eol);
            let trailing = content.is_empty() || content.ends_with('\n');
            if trailing && !model.is_empty() {
                expected.push_str(eol);
            }
            assert_eq!(String::from_utf8(written(&e, &f)).unwrap(), expected, "round {round}");
        }
    }

    #[test]
    fn save_replaces_through_a_temporary() {
        let (f, path) = file("save.txt", b"keep\nchange\n");
        let mut e = Edits::new(f.line_count());
        e.set_line(1, "changed".into());
        let temporary = write_temporary(&e, &f, &path).unwrap();
        drop(f);
        finish_save(&temporary, &path, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"keep\nchanged\n");
        assert!(!temporary.exists());
    }

    #[test]
    fn search_follows_edits() {
        let (f, _) = file("find.txt", b"apple\nbanana\napple pie\ncherry\n");
        let mut e = Edits::new(f.line_count());
        e.splice(0, 0, vec!["new apple".into()]); // everything moves down one
        e.splice(2, 3, vec![]); // drop "banana"
        let p = Pattern::new("apple", false).unwrap();
        let (hits, _) = find(&f, &e, &p, 100, &Progress::default()).unwrap();
        assert_eq!(
            hits,
            vec![Hit { line: 0, start: 4, end: 9 }, Hit { line: 1, start: 0, end: 5 }, Hit { line: 2, start: 0, end: 5 }]
        );
        assert_eq!(p.replace_all(b"apple apple", "pear", false), b"pear pear");
        assert_eq!(p.replace_at(b"apple apple", 6, "pear", false), b"apple pear");
        let re = Pattern::new(r"(\w+) (\w+)", true).unwrap();
        assert_eq!(re.replace_all(b"apple pie", "$2 $1", true), b"pie apple");
        assert_eq!(re.replace_all(b"apple pie", "$2 $1", false), b"$2 $1");
    }
}
