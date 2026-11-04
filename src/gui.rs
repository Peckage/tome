//! Desktop window (egui).
//!
//! Opening a file maps it, counts its lines and guesses the record tag; then
//! every record's byte range is indexed in parallel. The record list only
//! parses what is on screen, so millions of records scroll instantly. All
//! slow work (open, index, search, convert) runs on background threads and
//! reports through [`Progress`] atomics; the UI never blocks.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, FontId, Key, Rect, RichText, Sense, Stroke, Vec2};

use crate::cli::group;
use crate::engine::{self, Doc, Error, Format, Kind, Progress};
use crate::node::{Node, Options};
use crate::text::TextFile;

mod textview;
use textview::TextView;

pub const APP_ID: &str = "io.github.peckage.Tome";
const ROW_HEIGHT: f32 = 20.0;
const INDENT: f32 = 18.0;
const MAX_VALUE_CHARS: usize = 400;

pub fn run(path: Option<PathBuf>) -> i32 {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id(APP_ID)
            .with_title("Tome")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([720.0, 420.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    let result = eframe::run_native(
        "Tome",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.global_style_mut(|s| {
                s.spacing.item_spacing = Vec2::new(8.0, 6.0);
                s.spacing.button_padding = Vec2::new(10.0, 4.0);
            });
            let mut app = App::default();
            if let Some(path) = path {
                app.open(path, &cc.egui_ctx);
            }
            Ok(Box::new(app))
        }),
    );
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: could not open a window: {e}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// Background jobs
// ---------------------------------------------------------------------------

struct Job<T> {
    rx: Receiver<T>,
    progress: Arc<Progress>,
    started: Instant,
}

impl<T: Send + 'static> Job<T> {
    fn spawn(ctx: &egui::Context, work: impl FnOnce(&Progress) -> T + Send + 'static) -> Self {
        let (tx, rx) = channel();
        let progress = Arc::new(Progress::default());
        let p = progress.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = work(&p);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        Job { rx, progress, started: Instant::now() }
    }

    /// `Some` once the job has finished.
    fn poll(&self) -> Option<T> {
        match self.rx.try_recv() {
            Ok(v) => Some(v),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    fn cancel(&self) {
        self.progress.cancel.store(true, Ordering::Relaxed);
    }
}

struct Opened {
    path: PathBuf,
    doc: engine::Result<Arc<Doc>>,
    guessed_tag: Option<String>,
    seconds: f64,
}

struct TextOpened {
    path: PathBuf,
    text: engine::Result<Arc<TextFile>>,
    seconds: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Text,
    Records,
}

struct Indexed {
    tag: String,
    ranges: engine::Result<(Vec<(usize, usize)>, String)>,
    seconds: f64,
}

struct Searched {
    needle: String,
    hits: engine::Result<Vec<u32>>,
    seconds: f64,
}

struct Converted {
    output: PathBuf,
    result: engine::Result<u64>,
    seconds: f64,
}

// ---------------------------------------------------------------------------
// Tree rows
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Row {
    depth: usize,
    key: String,
    value: String,
    node: usize,
    has_children: bool,
    collapsed: bool,
}

/// Rows of one expanded record, below its header row.
struct Expanded {
    rows: Vec<Row>,
    collapsed: HashSet<usize>,
}

fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_VALUE_CHARS + 8));
    for (i, c) in s.chars().enumerate() {
        if i >= MAX_VALUE_CHARS {
            out.push('…');
            break;
        }
        match c {
            '\n' => out.push('⏎'),
            '\r' | '\t' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// A scalar as shown in the tree, or the text of `{"#text": v}`.
fn only_text(node: &Node) -> Option<String> {
    match node {
        Node::Obj(entries) => match entries.as_slice() {
            [(k, v)] if k == "#text" && v.is_scalar() => only_text(v),
            _ => None,
        },
        Node::Null => Some("null".into()),
        n if n.is_scalar() => n.scalar_text().map(|t| one_line(&t)),
        _ => None,
    }
}

/// "id=00123  code=A-B  name=Bolt" for a collapsed object.
fn summary(node: &Node, budget: usize) -> String {
    let entries = match node {
        Node::Obj(entries) => entries,
        Node::Arr(items) => return format!("[{} items]", items.len()),
        scalar => return only_text(scalar).unwrap_or_default(),
    };
    let mut out = String::new();
    for (key, value) in entries {
        if out.len() > budget {
            out.push_str(" …");
            break;
        }
        let text = match value {
            Node::Arr(items) if items.len() == 1 => match only_text(&items[0]) {
                Some(t) => t,
                None => continue,
            },
            Node::Arr(items) => format!("[{}]", items.len()),
            other => match only_text(other) {
                Some(t) => t,
                None => continue,
            },
        };
        if !out.is_empty() {
            out.push_str("   ");
        }
        if key != "#text" {
            out.push_str(key);
            out.push('=');
        }
        out.push_str(&text);
    }
    out
}

/// Flatten a record into display rows. Single-item lists are shown without
/// an index level and `{"#text": v}` as a plain value, so the tree reads like
/// "key: value" per line; the JSON panel shows the exact output.
fn build_rows(record: &Node, collapsed: &HashSet<usize>) -> Vec<Row> {
    struct Walk<'c> {
        rows: Vec<Row>,
        node: usize,
        collapsed: &'c HashSet<usize>,
    }
    impl Walk<'_> {
        /// `hidden` keeps numbering identical for folded branches.
        fn entry(&mut self, label: String, value: &Node, depth: usize, hidden: bool) {
            match value {
                Node::Arr(items) if items.is_empty() => self.item(label, value, depth, hidden),
                Node::Arr(items) => {
                    let many = items.len() > 1;
                    for (i, item) in items.iter().enumerate() {
                        let label = if many { format!("{label} [{}]", i + 1) } else { label.clone() };
                        self.item(label, item, depth, hidden);
                    }
                }
                other => self.item(label, other, depth, hidden),
            }
        }

        fn item(&mut self, key: String, value: &Node, depth: usize, hidden: bool) {
            self.node += 1;
            let id = self.node;
            let leaf = |value: String| Row { depth, key: key.clone(), value, node: id, has_children: false, collapsed: false };
            let row = if let Some(text) = only_text(value) {
                leaf(text)
            } else {
                match value {
                    Node::Obj(e) if e.is_empty() => leaf(String::new()),
                    Node::Arr(a) if a.is_empty() => leaf("[]".into()),
                    _ => {
                        let folded = self.collapsed.contains(&id);
                        let row = Row {
                            depth,
                            key: key.clone(),
                            value: if folded { summary(value, 160) } else { String::new() },
                            node: id,
                            has_children: true,
                            collapsed: folded,
                        };
                        if !hidden {
                            self.rows.push(row);
                        }
                        self.children(value, depth + 1, hidden || folded);
                        return;
                    }
                }
            };
            if !hidden {
                self.rows.push(row);
            }
        }

        fn children(&mut self, value: &Node, depth: usize, hidden: bool) {
            match value {
                Node::Obj(entries) => {
                    for (k, v) in entries {
                        self.entry(k.to_string(), v, depth, hidden);
                    }
                }
                Node::Arr(items) => {
                    for (i, v) in items.iter().enumerate() {
                        self.item(format!("[{}]", i + 1), v, depth, hidden);
                    }
                }
                _ => {}
            }
        }
    }
    let mut walk = Walk { rows: Vec::new(), node: 0, collapsed };
    match record {
        Node::Obj(_) | Node::Arr(_) => walk.children(record, 1, false),
        scalar => walk.item("value".into(), scalar, 1, false),
    }
    walk.rows
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Status {
    text: String,
    error: bool,
}

pub struct App {
    path: Option<PathBuf>,
    doc: Option<Arc<Doc>>,
    lines: Option<u64>,
    open_job: Option<Job<Opened>>,
    /// XML or JSON: the record view and converter apply.
    kind: Option<Kind>,
    options: Options,
    /// The "Drop fields" box, applied on Enter.
    drop_text: String,

    text: Option<Arc<TextFile>>,
    text_job: Option<Job<TextOpened>>,
    textview: TextView,
    view: View,
    /// The user picked a view for this file; don't switch automatically.
    view_chosen: bool,

    tag: String,
    /// The tag we last filled in automatically; anything else was typed.
    auto_tag: String,
    tag_edited_at: Option<Instant>,

    ranges: Option<Arc<Vec<(usize, usize)>>>,
    indexed_tag: String,
    index_job: Option<Job<Indexed>>,

    /// `None` shows all records; `Some` shows search hits (record indices).
    hits: Option<Arc<Vec<u32>>>,
    search: String,
    searched_for: String,
    search_job: Option<Job<Searched>>,
    goto: String,

    top: usize,
    scroll_remainder: f32,
    expanded: BTreeMap<u32, Expanded>,
    selected: Option<u32>,
    selected_json: Option<(u32, String)>,
    summaries: HashMap<u32, String>,

    format: Format,
    output: String,
    confirm_overwrite: bool,
    convert_job: Option<Job<Converted>>,
    dialog: Option<Receiver<DialogResult>>,

    status: Status,
}

enum DialogResult {
    Open(Option<PathBuf>),
    Save(Option<PathBuf>),
}

impl Default for App {
    fn default() -> Self {
        App {
            path: None,
            doc: None,
            lines: None,
            open_job: None,
            kind: None,
            options: Options::default(),
            drop_text: String::new(),
            text: None,
            text_job: None,
            textview: TextView::default(),
            view: View::Text,
            view_chosen: false,
            tag: "part".into(),
            auto_tag: "part".into(),
            tag_edited_at: None,
            ranges: None,
            indexed_tag: String::new(),
            index_job: None,
            hits: None,
            search: String::new(),
            searched_for: String::new(),
            search_job: None,
            goto: String::new(),
            top: 0,
            scroll_remainder: 0.0,
            expanded: BTreeMap::new(),
            selected: None,
            selected_json: None,
            summaries: HashMap::new(),
            format: Format::Pretty,
            output: String::new(),
            confirm_overwrite: false,
            convert_job: None,
            dialog: None,
            status: Status { text: "Open a file (Ctrl+O), or drop one on this window.".into(), error: false },
        }
    }
}

fn human_size(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", units[unit]) }
}

impl App {
    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = Status { text: text.into(), error };
    }

    pub fn open(&mut self, path: PathBuf, ctx: &egui::Context) {
        if let Some(job) = self.convert_job.as_ref() {
            job.cancel();
        }
        self.doc = None;
        self.lines = None;
        self.reset_view();
        self.ranges = None;
        self.indexed_tag.clear();
        self.index_job = None;
        self.open_job = None;
        self.text = None;
        self.textview.reset();
        self.view_chosen = false;
        self.path = Some(path.clone());
        let kind = engine::sniff_kind(&path);
        if let Some(k) = kind {
            // Keep a tag the user typed, unless it was for the other kind of
            // file: an XML tag means nothing as a JSON path and vice versa.
            let typed = self.tag.trim() != self.auto_tag;
            let switched = self.kind.is_some_and(|old| old != k);
            if !typed || switched {
                self.tag = k.default_selector().into();
                self.auto_tag = self.tag.clone();
            }
        }
        self.kind = kind;
        self.view = View::Text;
        self.output = default_output(&path, self.format);
        self.set_status(format!("Opening {}…", path.display()), false);
        let text_path = path.clone();
        self.text_job = Some(Job::spawn(ctx, move |_| {
            let started = Instant::now();
            let text = TextFile::open(&text_path).map(Arc::new);
            TextOpened { path: text_path, text, seconds: started.elapsed().as_secs_f64() }
        }));
        if let Some(kind) = kind {
            self.open_job = Some(Job::spawn(ctx, move |_| {
                let started = Instant::now();
                let doc = Doc::open_as(&path, kind).map(Arc::new);
                let guessed_tag = doc.as_ref().ok().and_then(|d| d.guess_record_tag());
                Opened { path, doc, guessed_tag, seconds: started.elapsed().as_secs_f64() }
            }));
        }
    }

    fn reset_view(&mut self) {
        self.hits = None;
        self.searched_for.clear();
        self.search_job = None;
        self.top = 0;
        self.scroll_remainder = 0.0;
        self.expanded.clear();
        self.selected = None;
        self.selected_json = None;
        self.summaries.clear();
    }

    fn start_index(&mut self, ctx: &egui::Context) {
        let Some(doc) = self.doc.clone() else { return };
        let tag = self.tag.trim().to_string();
        if tag.is_empty() && doc.kind == Kind::Xml {
            return;
        }
        self.reset_view();
        self.ranges = None;
        self.indexed_tag = tag.clone();
        self.index_job = Some(Job::spawn(ctx, move |_| {
            let started = Instant::now();
            let ranges = engine::index(&doc, &tag);
            Indexed { tag, ranges, seconds: started.elapsed().as_secs_f64() }
        }));
    }

    fn start_search(&mut self, ctx: &egui::Context) {
        let (Some(doc), Some(ranges)) = (self.doc.clone(), self.ranges.clone()) else { return };
        if let Some(job) = &self.search_job {
            job.cancel();
        }
        let needle = self.search.clone();
        if needle.is_empty() {
            self.hits = None;
            self.searched_for.clear();
            self.search_job = None;
            self.after_view_change();
            return;
        }
        let options = self.options.clone();
        self.search_job = Some(Job::spawn(ctx, move |progress| {
            let started = Instant::now();
            let hits = engine::search(&doc, &ranges, &needle, &options, progress);
            Searched { needle, hits, seconds: started.elapsed().as_secs_f64() }
        }));
    }

    fn after_view_change(&mut self) {
        self.top = 0;
        self.scroll_remainder = 0.0;
        self.expanded.clear();
    }

    fn start_convert(&mut self, ctx: &egui::Context, overwrite_ok: bool) {
        let Some(doc) = self.doc.clone() else { return };
        let output = PathBuf::from(self.output.trim());
        if output.as_os_str().is_empty() {
            self.set_status("Choose an output file.", true);
            return;
        }
        if output.exists() && !overwrite_ok {
            self.confirm_overwrite = true;
            return;
        }
        self.confirm_overwrite = false;
        let tag = self.indexed_tag.clone();
        let format = self.format;
        let options = self.options.clone();
        self.set_status("Converting…", false);
        self.convert_job = Some(Job::spawn(ctx, move |progress| {
            let started = Instant::now();
            let result = engine::convert(&doc, &tag, &output, format, &options, progress);
            Converted { output, result, seconds: started.elapsed().as_secs_f64() }
        }));
    }

    fn view_len(&self) -> usize {
        match (&self.hits, &self.ranges) {
            (Some(h), _) => h.len(),
            (None, Some(r)) => r.len(),
            _ => 0,
        }
    }

    fn record_at(&self, view_pos: usize) -> u32 {
        match &self.hits {
            Some(h) => h[view_pos],
            None => view_pos as u32,
        }
    }

    fn view_pos_of(&self, record: u32) -> Option<usize> {
        match &self.hits {
            Some(h) => h.binary_search(&record).ok(),
            None => Some(record as usize),
        }
    }

    /// Expanded records in view order with their extra row counts.
    fn segments(&self) -> Vec<(usize, usize)> {
        let mut segs: Vec<(usize, usize)> = self
            .expanded
            .iter()
            .filter_map(|(&rec, e)| self.view_pos_of(rec).map(|p| (p, e.rows.len())))
            .collect();
        segs.sort_unstable();
        segs
    }

    fn total_rows(&self, segs: &[(usize, usize)]) -> usize {
        self.view_len() + segs.iter().map(|s| s.1).sum::<usize>()
    }

    /// Map a flat row to (view position, None for the record row or Some(i)
    /// for row i of its expansion).
    fn locate(&self, row: usize, segs: &[(usize, usize)]) -> (usize, Option<usize>) {
        let mut extra = 0usize;
        for &(pos, n) in segs {
            let record_row = pos + extra;
            if row < record_row {
                break;
            }
            if row == record_row {
                return (pos, None);
            }
            if row <= record_row + n {
                return (pos, Some(row - record_row - 1));
            }
            extra += n;
        }
        (row - extra, None)
    }

    fn flat_row_of(&self, view_pos: usize, segs: &[(usize, usize)]) -> usize {
        view_pos + segs.iter().filter(|s| s.0 < view_pos).map(|s| s.1).sum::<usize>()
    }

    fn parse_record(&self, record: u32) -> Option<Node<'_>> {
        let doc = self.doc.as_ref()?;
        let range = *self.ranges.as_ref()?.get(record as usize)?;
        engine::parse_node(doc, range, &self.options).ok()
    }

    /// The clean-up options changed: everything shown per record is stale.
    fn options_changed(&mut self, ctx: &egui::Context) {
        self.summaries.clear();
        let open: Vec<u32> = self.expanded.keys().copied().collect();
        self.expanded.clear();
        for record in open {
            self.toggle_record(record);
        }
        if let Some(record) = self.selected {
            self.selected_json = None;
            self.select(record);
        }
        if !self.searched_for.is_empty() {
            self.search = self.searched_for.clone();
            self.start_search(ctx);
        }
    }

    fn toggle_record(&mut self, record: u32) {
        if self.expanded.remove(&record).is_some() {
            return;
        }
        let collapsed = HashSet::new();
        if let Some(obj) = self.parse_record(record) {
            let rows = build_rows(&obj, &collapsed);
            self.expanded.insert(record, Expanded { rows, collapsed });
        }
    }

    fn toggle_node(&mut self, record: u32, node: usize) {
        let Some(mut e) = self.expanded.remove(&record) else { return };
        if !e.collapsed.remove(&node) {
            e.collapsed.insert(node);
        }
        if let Some(obj) = self.parse_record(record) {
            e.rows = build_rows(&obj, &e.collapsed);
        }
        self.expanded.insert(record, e);
    }

    fn select(&mut self, record: u32) {
        self.selected = Some(record);
        if self.selected_json.as_ref().is_some_and(|(r, _)| *r == record) {
            return;
        }
        let (Some(doc), Some(ranges)) = (&self.doc, &self.ranges) else { return };
        if let Some(&range) = ranges.get(record as usize) {
            let json = engine::record_json(doc, range, true, &self.options).unwrap_or_else(|e| format!("error: {e}"));
            self.selected_json = Some((record, json));
        }
    }

    fn summary_of(&mut self, record: u32) -> String {
        if let Some(s) = self.summaries.get(&record) {
            return s.clone();
        }
        let s = match self.parse_record(record) {
            Some(obj) => summary(&obj, 200),
            None => "(could not parse)".into(),
        };
        if self.summaries.len() > 4096 {
            self.summaries.clear();
        }
        self.summaries.insert(record, s.clone());
        s
    }

    // -----------------------------------------------------------------------
    // Job results
    // -----------------------------------------------------------------------

    fn poll_jobs(&mut self, ctx: &egui::Context) {
        if let Some(done) = self.text_job.as_ref().and_then(Job::poll) {
            self.text_job = None;
            if Some(&done.path) == self.path.as_ref() {
                match done.text {
                    Ok(text) => {
                        self.lines = Some(text.line_count() as u64);
                        if self.kind.is_none() {
                            self.set_status(
                                format!("Opened {} lines in {:.2} s.", group(text.line_count() as u64), done.seconds),
                                false,
                            );
                        }
                        self.text = Some(text);
                    }
                    Err(e) => self.set_status(format!("Cannot read this file: {e}"), true),
                }
            }
        }

        if let Some(done) = self.open_job.as_ref().and_then(Job::poll) {
            self.open_job = None;
            if Some(&done.path) == self.path.as_ref() {
                match done.doc {
                    Ok(doc) => {
                        let note = doc.transcoded_from.as_ref().map(|e| format!(" (converted from {e})")).unwrap_or_default();
                        self.doc = Some(doc);
                        if let Some(tag) = done.guessed_tag {
                            let typed = !self.tag.trim().is_empty() && self.tag.trim() != self.auto_tag;
                            if !typed {
                                self.tag = tag.clone();
                                self.auto_tag = tag;
                            }
                        }
                        self.set_status(format!("Opened in {:.2} s{note}.", done.seconds), false);
                        self.start_index(ctx);
                    }
                    Err(e) => {
                        let kind = self.kind.map_or("XML", Kind::name);
                        self.set_status(format!("Not readable as {kind}: {e}. The text view still works."), true);
                    }
                }
            }
        }

        if let Some(done) = self.index_job.as_ref().and_then(Job::poll) {
            self.index_job = None;
            if done.tag == self.indexed_tag {
                let is_json = self.doc.as_ref().is_some_and(|d| d.kind == Kind::Json);
                let ranges = done.ranges.map(|(r, used)| {
                    // An automatic JSON path: show which one was picked.
                    if is_json && done.tag.is_empty() {
                        self.tag = used.clone();
                        self.auto_tag = used.clone();
                        self.indexed_tag = used;
                    }
                    r
                });
                let what = if is_json { self.indexed_tag.clone() } else { format!("<{}>", done.tag) };
                match ranges {
                    Ok(r) if r.is_empty() => {
                        let hint = self
                            .doc
                            .as_ref()
                            .and_then(|d| d.guess_record_tag())
                            .filter(|t| *t != done.tag)
                            .map(|t| format!(" The first repeating element is <{t}>."))
                            .unwrap_or_default();
                        self.ranges = Some(Arc::new(r));
                        let message = if is_json { format!("No records found at \"{what}\".") } else { format!("No {what} elements found.{hint}") };
                        self.set_status(message, true);
                    }
                    Ok(r) => {
                        let n = r.len();
                        self.ranges = Some(Arc::new(r));
                        if !self.view_chosen {
                            self.view = View::Records;
                        }
                        self.set_status(
                            format!("Indexed {} {} records in {:.2} s.", group(n as u64), what, done.seconds),
                            false,
                        );
                    }
                    Err(e) => {
                        self.ranges = None;
                        let kind = self.kind.map_or("XML", Kind::name);
                        self.set_status(format!("{kind} error: {e}"), true);
                    }
                }
            }
        }

        if let Some(done) = self.search_job.as_ref().and_then(Job::poll) {
            self.search_job = None;
            match done.hits {
                Ok(hits) => {
                    let total = self.ranges.as_ref().map_or(0, |r| r.len());
                    self.set_status(
                        format!(
                            "{} of {} records contain \"{}\" ({:.2} s).",
                            group(hits.len() as u64),
                            group(total as u64),
                            done.needle,
                            done.seconds
                        ),
                        false,
                    );
                    self.hits = Some(Arc::new(hits));
                    self.searched_for = done.needle;
                    self.after_view_change();
                }
                Err(Error::Cancelled) => self.set_status("Search cancelled.", false),
                Err(e) => self.set_status(format!("Search failed: {e}"), true),
            }
        }

        if let Some(done) = self.convert_job.as_ref().and_then(Job::poll) {
            self.convert_job = None;
            match done.result {
                Ok(n) => {
                    let size = std::fs::metadata(&done.output).map(|m| human_size(m.len())).unwrap_or_default();
                    self.set_status(
                        format!("Wrote {} records ({size}) to {} in {:.2} s.", group(n), done.output.display(), done.seconds),
                        false,
                    );
                }
                Err(Error::Cancelled) => self.set_status("Cancelled. No output was written.", false),
                Err(e) => self.set_status(format!("Conversion failed: {e}"), true),
            }
        }

        if let Some(rx) = &self.dialog {
            if let Ok(result) = rx.try_recv() {
                self.dialog = None;
                match result {
                    DialogResult::Open(Some(path)) => self.open(path, ctx),
                    DialogResult::Save(Some(path)) => self.output = path.display().to_string(),
                    _ => {}
                }
            }
        }

        // Re-index shortly after the tag stops changing.
        if let Some(at) = self.tag_edited_at {
            if at.elapsed() > Duration::from_millis(400) {
                self.tag_edited_at = None;
                if self.tag.trim() != self.indexed_tag {
                    self.start_index(ctx);
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(100));
            }
        }

        if let Some((text, error)) = self.textview.status.take() {
            self.set_status(text, error);
        }
        let busy = self.open_job.is_some()
            || self.text_job.is_some()
            || self.index_job.is_some()
            || self.search_job.is_some()
            || self.convert_job.is_some()
            || self.textview.busy();
        if busy {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        let dir = self.path.as_ref().and_then(|p| p.parent().map(Path::to_path_buf));
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new().set_title("Open file").add_filter("All files", &["*"]).add_filter("XML", &["xml", "XML"]);
            if let Some(dir) = dir {
                dialog = dialog.set_directory(dir);
            }
            let _ = tx.send(DialogResult::Open(dialog.pick_file()));
            ctx.request_repaint();
        });
        self.dialog = Some(rx);
    }

    fn save_dialog(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        let current = PathBuf::from(&self.output);
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new().set_title("Save JSON as");
            if let Some(dir) = current.parent().filter(|d| !d.as_os_str().is_empty()) {
                dialog = dialog.set_directory(dir);
            }
            if let Some(name) = current.file_name() {
                dialog = dialog.set_file_name(name.to_string_lossy());
            }
            let _ = tx.send(DialogResult::Save(dialog.save_file()));
            ctx.request_repaint();
        });
        self.dialog = Some(rx);
    }

    // -----------------------------------------------------------------------
    // Drawing
    // -----------------------------------------------------------------------

    /// Everything inside the window. Separate from `eframe::App::ui` so tests
    /// can drive it headless.
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.poll_jobs(&ctx);

        let dropped = ui.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).find(|p| !p.as_os_str().is_empty()));
        if let Some(path) = dropped {
            self.open(path, &ctx);
        }

        if ui.input(|i| i.modifiers.command && i.key_pressed(Key::O)) && self.dialog.is_none() {
            self.open_dialog(&ctx);
        }

        egui::Panel::top("file").show(ui, |ui| {
            ui.add_space(6.0);
            self.file_bar(ui);
            if self.kind.is_some() {
                ui.add_space(2.0);
                self.convert_bar(ui);
                self.options_bar(ui);
            }
            ui.add_space(6.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(3.0);
            let color = if self.status.error { ui.visuals().error_fg_color } else { ui.visuals().text_color() };
            ui.label(RichText::new(&self.status.text).color(color));
            ui.add_space(3.0);
        });
        if self.selected.is_some() && self.view == View::Records {
            egui::Panel::right("json").resizable(true).default_size(420.0).min_size(240.0).show(ui, |ui| self.json_panel(ui));
        }
        egui::CentralPanel::default().show(ui, |ui| {
            if self.path.is_some() {
                ui.horizontal(|ui| {
                    if ui.selectable_label(self.view == View::Text, "Text").on_hover_text("Every line of the file").clicked() {
                        self.view = View::Text;
                        self.view_chosen = true;
                    }
                    if self.kind.is_some()
                        && ui
                            .selectable_label(self.view == View::Records, "Records")
                            .on_hover_text("One row per record, with the JSON it converts to")
                            .clicked()
                    {
                        self.view = View::Records;
                        self.view_chosen = true;
                    }
                });
                ui.add_space(2.0);
            }
            match self.view {
                View::Records => self.records_panel(ui),
                View::Text => match self.text.clone() {
                    Some(text) => self.textview.draw(ui, &text),
                    None => {
                        ui.centered_and_justified(|ui| {
                            let msg = if self.text_job.is_some() { "Reading…" } else { "Open a file, or drop one on this window. Any size, any text." };
                            ui.label(RichText::new(msg).weak().size(16.0));
                        });
                    }
                },
            }
        });
        if let Some((text, error)) = self.textview.status.take() {
            self.set_status(text, error);
        }
    }

    fn file_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            let busy_dialog = self.dialog.is_some();
            if ui.add_enabled(!busy_dialog, egui::Button::new("📂 Open…")).on_hover_text("Open any file (Ctrl+O)").clicked() {
                self.open_dialog(&ctx);
            }
            match &self.path {
                Some(p) => {
                    ui.label(RichText::new(p.display().to_string()).strong());
                    let mut info = Vec::new();
                    if let Some(doc) = &self.doc {
                        info.push(human_size(doc.file_size));
                    }
                    match self.lines {
                        Some(n) => info.push(format!("{} lines", group(n))),
                        None if self.text_job.is_some() => info.push("counting lines…".into()),
                        None => {}
                    }
                    if let Some(r) = &self.ranges {
                        info.push(format!("{} records", group(r.len() as u64)));
                    } else if self.index_job.is_some() || self.open_job.is_some() {
                        info.push("indexing records…".into());
                    }
                    ui.label(RichText::new(info.join("  ·  ")).weak());
                }
                None => {
                    ui.label(RichText::new("No file open").weak());
                }
            }
        });
    }

    fn convert_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let has_doc = self.doc.is_some();
        let converting = self.convert_job.is_some();
        ui.horizontal(|ui| {
            let json = self.kind == Some(Kind::Json);
            ui.label(if json { "Records at" } else { "Record tag" });
            let tag = ui
                .add_enabled(
                    !converting,
                    egui::TextEdit::singleline(&mut self.tag).desired_width(110.0).hint_text(if json { "auto" } else { "tag" }),
                )
                .on_hover_text(if json {
                    "Key path to the array of records, e.g. data.items. Empty: the root array or the biggest array under the root. $: the whole file as one record. lines: one record per line (NDJSON)."
                } else {
                    "The XML element that is one record"
                });
            if tag.changed() {
                self.tag_edited_at = Some(Instant::now());
            }
            if tag.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                self.tag_edited_at = None;
                if self.tag.trim() != self.indexed_tag {
                    self.start_index(&ctx);
                }
            }
            ui.separator();
            ui.label("Format");
            let before = self.format;
            ui.add_enabled_ui(!converting, |ui| {
                egui::ComboBox::from_id_salt("format")
                    .selected_text(self.format.name())
                    .show_ui(ui, |ui| {
                        for f in Format::ALL {
                            ui.selectable_value(&mut self.format, f, f.name());
                        }
                    });
            });
            if self.format != before {
                if let Some((stem, ext)) = self.output.rsplit_once('.') {
                    let known = ext == "json" || Format::from_extension(ext).is_some();
                    if known && !stem.ends_with(['/', '\\']) {
                        let input = self.path.clone();
                        self.output = format!("{stem}.{}", self.format.extension());
                        if let Some(input) = input.filter(|p| Path::new(&self.output) == p.as_path()) {
                            self.output = default_output(&input, self.format);
                        }
                    }
                }
            }
            ui.separator();
            ui.label("Output");
            ui.add_enabled(!converting, egui::TextEdit::singleline(&mut self.output).desired_width(ui.available_width() - 260.0));
            if ui.add_enabled(!converting && self.dialog.is_none() && has_doc, egui::Button::new("…")).on_hover_text("Choose output file").clicked() {
                self.save_dialog(&ctx);
            }
            if converting {
                if ui.button("Cancel").clicked() {
                    if let Some(job) = &self.convert_job {
                        job.cancel();
                    }
                }
            } else if ui.add_enabled(has_doc, egui::Button::new(RichText::new("Convert").strong())).clicked() {
                self.start_convert(&ctx, false);
            }
        });
        if self.confirm_overwrite {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} already exists.", self.output)).color(ui.visuals().warn_fg_color));
                if ui.button("Replace it").clicked() {
                    self.start_convert(&ctx, true);
                }
                if ui.button("Keep it").clicked() {
                    self.confirm_overwrite = false;
                }
            });
        }
        if let (Some(job), Some(doc)) = (&self.convert_job, &self.doc) {
            let fraction = job.progress.bytes.load(Ordering::Relaxed) as f32 / doc.bytes().len().max(1) as f32;
            let records = job.progress.records.load(Ordering::Relaxed);
            ui.add(
                egui::ProgressBar::new(fraction)
                    .text(format!("{:.0}%  ·  {} records  ·  {:.1} s", fraction * 100.0, group(records), job.started.elapsed().as_secs_f32())),
            );
        }
    }

    /// Clean-up options: they change the converted output, the JSON panel,
    /// the tree and what search matches.
    fn options_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let converting = self.convert_job.is_some();
        let before = self.options.clone();
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!converting, |ui| {
                ui.label("Clean up");
                ui.checkbox(&mut self.options.trim, "Trim").on_hover_text("Remove spaces at the start and end of every value");
                ui.checkbox(&mut self.options.collapse_spaces, "Collapse spaces").on_hover_text("Turn runs of spaces, tabs and newlines into one space");
                ui.checkbox(&mut self.options.remove_empty, "Remove empty").on_hover_text("Drop empty values, nulls, {} and [] everywhere (records themselves are kept)");
                ui.separator();
                ui.label("Shape");
                ui.checkbox(&mut self.options.unwrap_single, "Unwrap 1-item lists").on_hover_text("[x] becomes x");
                ui.checkbox(&mut self.options.text_values, "Text as value").on_hover_text("{\"#text\": v} becomes v");
                if self.kind == Some(Kind::Xml) {
                    ui.label("Attr. prefix");
                    ui.add(egui::TextEdit::singleline(&mut self.options.attr_prefix).desired_width(28.0).hint_text("@"))
                        .on_hover_text("Put this in front of attribute names, e.g. @ (xmltodict style)");
                }
                ui.separator();
                ui.label("Drop fields");
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.drop_text)
                        .desired_width(150.0)
                        .hint_text("key1, key2 (Enter)"),
                );
                if field.lost_focus() {
                    self.options.set_drop_keys(&self.drop_text);
                }
                ui.separator();
                if ui.button("Simple").on_hover_text("xmltodict style: @attributes, plain text values, no one-item lists").clicked() {
                    let keep = (self.options.trim, self.options.collapse_spaces, self.options.remove_empty, self.options.drop_keys.clone());
                    self.options = Options::simple();
                    (self.options.trim, self.options.collapse_spaces, self.options.remove_empty, self.options.drop_keys) = keep;
                    if self.kind != Some(Kind::Xml) {
                        self.options.attr_prefix.clear();
                    }
                }
                if ui.add_enabled(!self.options.is_plain(), egui::Button::new("Reset")).on_hover_text("Back to the plain mapping").clicked() {
                    self.options = Options::default();
                    self.drop_text.clear();
                }
            });
        });
        if self.options != before {
            self.options_changed(&ctx);
        }
    }

    fn search_bar(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.horizontal(|ui| {
            let enabled = self.ranges.as_ref().is_some_and(|r| !r.is_empty());
            let field = ui.add_enabled(
                enabled,
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search all records (Enter). Case-insensitive unless you type capitals.")
                    .desired_width(ui.available_width() - 300.0),
            );
            if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                self.start_search(&ctx);
            }
            if self.search_job.is_some() {
                if ui.button("Stop").clicked() {
                    if let Some(job) = &self.search_job {
                        job.cancel();
                    }
                }
            } else if ui.add_enabled(enabled, egui::Button::new("Search")).clicked() {
                self.start_search(&ctx);
            }
            if self.hits.is_some() && ui.button("Show all").clicked() {
                self.search.clear();
                self.hits = None;
                self.searched_for.clear();
                self.after_view_change();
            }
            ui.separator();
            ui.label("Go to #");
            let goto = ui.add_enabled(enabled, egui::TextEdit::singleline(&mut self.goto).desired_width(80.0));
            if goto.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                if let Ok(n) = self.goto.trim().replace(',', "").parse::<usize>() {
                    let record = n.saturating_sub(1) as u32;
                    let pos = match &self.hits {
                        Some(h) => h.partition_point(|&x| x < record),
                        None => record as usize,
                    };
                    let segs = self.segments();
                    let len = self.view_len();
                    if len > 0 {
                        self.top = self.flat_row_of(pos.min(len - 1), &segs);
                        self.select(self.record_at(pos.min(len - 1)));
                    }
                }
            }
        });
        if let (Some(job), Some(ranges)) = (&self.search_job, &self.ranges) {
            let done = job.progress.records.load(Ordering::Relaxed) as f32 / ranges.len().max(1) as f32;
            ui.add(egui::ProgressBar::new(done).text(format!("Searching… {:.0}%", done * 100.0)));
        }
    }

    fn records_panel(&mut self, ui: &mut egui::Ui) {
        self.search_bar(ui);
        ui.add_space(4.0);

        if self.ranges.is_none() {
            ui.centered_and_justified(|ui| {
                let text = if self.index_job.is_some() || self.open_job.is_some() {
                    "Reading…"
                } else if self.doc.is_some() {
                    "No records to show."
                } else {
                    "Open an XML file to browse its records."
                };
                ui.label(RichText::new(text).weak().size(16.0));
            });
            return;
        }

        let segs = self.segments();
        let total = self.total_rows(&segs);
        let area = ui.available_rect_before_wrap();
        let scrollbar_w = 12.0;
        let list = Rect::from_min_max(area.min, egui::pos2(area.max.x - scrollbar_w - 2.0, area.max.y));
        let bar = Rect::from_min_max(egui::pos2(area.max.x - scrollbar_w, area.min.y), area.max);
        let visible = ((list.height() / ROW_HEIGHT).floor() as usize).max(1);
        let max_top = total.saturating_sub(visible);

        // Mouse wheel and keyboard.
        let list_response = ui.interact(list, ui.scope_id().with("records"), Sense::click());
        if ui.rect_contains_pointer(list) {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            self.scroll_remainder -= dy;
            let rows = (self.scroll_remainder / ROW_HEIGHT).trunc();
            if rows != 0.0 {
                self.scroll_remainder -= rows * ROW_HEIGHT;
                self.top = (self.top as i64 + rows as i64).clamp(0, max_top as i64) as usize;
            }
        }
        let typing = ui.ctx().memory(|m| m.focused().is_some());
        if !typing {
            let (down, up, pgdn, pgup, home, end) = ui.input(|i| {
                (
                    i.key_pressed(Key::ArrowDown),
                    i.key_pressed(Key::ArrowUp),
                    i.key_pressed(Key::PageDown),
                    i.key_pressed(Key::PageUp),
                    i.key_pressed(Key::Home),
                    i.key_pressed(Key::End),
                )
            });
            let page = visible.saturating_sub(1).max(1);
            if down { self.top = (self.top + 1).min(max_top); }
            if up { self.top = self.top.saturating_sub(1); }
            if pgdn { self.top = (self.top + page).min(max_top); }
            if pgup { self.top = self.top.saturating_sub(page); }
            if home { self.top = 0; }
            if end { self.top = max_top; }
        }
        self.top = self.top.min(max_top);

        // Rows.
        let painter = ui.painter_at(list);
        let visuals = ui.visuals().clone();
        let mono = FontId::monospace(13.0);
        let key_color = if visuals.dark_mode { Color32::from_rgb(130, 190, 255) } else { Color32::from_rgb(20, 90, 170) };
        let value_color = if visuals.dark_mode { Color32::from_rgb(220, 200, 150) } else { Color32::from_rgb(130, 80, 10) };
        let weak = visuals.weak_text_color();
        let mut clicked: Option<(usize, Option<usize>)> = None;
        let pointer = ui.input(|i| i.pointer.interact_pos());
        let primary_clicked = list_response.clicked();

        for i in 0..visible.min(total.saturating_sub(self.top)) {
            let flat = self.top + i;
            let (pos, sub) = self.locate(flat, &segs);
            let record = self.record_at(pos);
            let y = list.min.y + i as f32 * ROW_HEIGHT;
            let row_rect = Rect::from_min_size(egui::pos2(list.min.x, y), Vec2::new(list.width(), ROW_HEIGHT));
            let hovered = pointer.is_some_and(|p| row_rect.contains(p));
            if self.selected == Some(record) && sub.is_none() {
                painter.rect_filled(row_rect, 3.0, visuals.selection.bg_fill.gamma_multiply(0.6));
            } else if hovered {
                painter.rect_filled(row_rect, 3.0, visuals.widgets.hovered.weak_bg_fill.gamma_multiply(0.5));
            }
            if hovered && primary_clicked {
                clicked = Some((pos, sub));
            }
            let text_y = y + ROW_HEIGHT / 2.0;
            match sub {
                None => {
                    let open = self.expanded.contains_key(&record);
                    let mut x = list.min.x + 4.0;
                    painter.text(egui::pos2(x, text_y), Align2::LEFT_CENTER, if open { "▾" } else { "▸" }, mono.clone(), weak);
                    x += 16.0;
                    let number = format!("#{}", group(record as u64 + 1));
                    let r = painter.text(egui::pos2(x, text_y), Align2::LEFT_CENTER, &number, mono.clone(), weak);
                    x = r.max.x + 10.0;
                    let r = painter.text(egui::pos2(x, text_y), Align2::LEFT_CENTER, &self.indexed_tag, mono.clone(), key_color);
                    x = r.max.x + 12.0;
                    if !open {
                        let s = self.summary_of(record);
                        painter.text(egui::pos2(x, text_y), Align2::LEFT_CENTER, s, mono.clone(), visuals.text_color());
                    }
                }
                Some(k) => {
                    let row = &self.expanded[&record].rows[k];
                    let mut x = list.min.x + 4.0 + row.depth as f32 * INDENT;
                    if row.has_children {
                        painter.text(egui::pos2(x, text_y), Align2::LEFT_CENTER, if row.collapsed { "▸" } else { "▾" }, mono.clone(), weak);
                    }
                    x += 16.0;
                    let key_text = if row.has_children || row.value.is_empty() { row.key.clone() } else { format!("{}:", row.key) };
                    let r = painter.text(egui::pos2(x, text_y), Align2::LEFT_CENTER, key_text, mono.clone(), key_color);
                    x = r.max.x + 8.0;
                    let color = if row.has_children { visuals.text_color() } else { value_color };
                    painter.text(egui::pos2(x, text_y), Align2::LEFT_CENTER, &row.value, mono.clone(), color);
                }
            }
        }

        if let Some((pos, sub)) = clicked {
            let record = self.record_at(pos);
            match sub {
                None => {
                    self.toggle_record(record);
                    self.select(record);
                }
                Some(k) => {
                    let (node, has_children) = {
                        let row = &self.expanded[&record].rows[k];
                        (row.node, row.has_children)
                    };
                    if has_children {
                        self.toggle_node(record, node);
                    }
                    self.select(record);
                }
            }
        }

        if let Some(top) = scrollbar(ui, bar, self.top, total, visible, ui.scope_id().with("scrollbar")) {
            self.top = top;
        }
        let painter = ui.painter();
        painter.rect_stroke(list, 4.0, Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color), egui::StrokeKind::Outside);
        ui.allocate_rect(area, Sense::hover());
    }

    fn json_panel(&mut self, ui: &mut egui::Ui) {
        let Some(record) = self.selected else { return };
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Record #{}", group(record as u64 + 1))).strong());
            ui.label(RichText::new("exact JSON output").weak());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("✕").on_hover_text("Close").clicked() {
                    self.selected = None;
                }
                if ui.button("Copy").clicked() {
                    if let Some((_, json)) = &self.selected_json {
                        ui.ctx().copy_text(json.clone());
                        self.status = Status { text: "Copied the record's JSON.".into(), error: false };
                    }
                }
            });
        });
        ui.separator();
        if let Some((_, json)) = &self.selected_json {
            egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                let mut text = json.as_str();
                ui.add(egui::TextEdit::multiline(&mut text).code_editor().desired_width(f32::INFINITY));
            });
        }
    }
}

/// A vertical scrollbar for a list of `total` rows showing `visible` from
/// `top`. Returns a new `top` when the user dragged or clicked it.
fn scrollbar(ui: &mut egui::Ui, bar: Rect, top: usize, total: usize, visible: usize, id: egui::Id) -> Option<usize> {
    let response = ui.interact(bar, id, Sense::click_and_drag());
    let visuals = ui.visuals().clone();
    let painter = ui.painter();
    painter.rect_filled(bar, 6.0, visuals.extreme_bg_color);
    if total <= visible {
        return None;
    }
    let max_top = total - visible;
    let thumb_h = (bar.height() * visible as f32 / total as f32).max(24.0);
    let travel = bar.height() - thumb_h;
    let t = top as f32 / max_top.max(1) as f32;
    let thumb = Rect::from_min_size(egui::pos2(bar.min.x, bar.min.y + t * travel), Vec2::new(bar.width(), thumb_h));
    let color = if response.dragged() || response.hovered() { visuals.widgets.active.bg_fill } else { visuals.widgets.inactive.bg_fill };
    painter.rect_filled(thumb.shrink(2.0), 5.0, color);
    let p = response.interact_pointer_pos()?;
    if response.dragged() || response.clicked() {
        let t = ((p.y - bar.min.y - thumb_h / 2.0) / travel.max(1.0)).clamp(0.0, 1.0);
        return Some((t * max_top as f32).round() as usize);
    }
    None
}

/// `parts.xml` → `parts.json`; when that would be the input itself
/// (`data.json` → JSON) it becomes `data-converted.json`.
fn default_output(input: &Path, format: Format) -> String {
    let out = input.with_extension(format.extension());
    if out == input {
        let stem = input.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        return input.with_file_name(format!("{stem}-converted.{}", format.extension())).display().to_string();
    }
    out.display().to_string()
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Let a running conversion remove its temporary file before we exit.
        if let Some(job) = &self.convert_job {
            job.cancel();
            let _ = job.rx.recv_timeout(Duration::from_secs(10));
        }
    }
}

// ---------------------------------------------------------------------------
// Headless test hooks
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ctx: &egui::Context, app: &mut App, events: Vec<egui::Event>) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1200.0, 800.0))),
            events,
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| app.draw(ui));
    }

    fn wait(ctx: &egui::Context, app: &mut App, mut done: impl FnMut(&App) -> bool) {
        let start = Instant::now();
        while !done(app) {
            assert!(start.elapsed() < Duration::from_secs(30), "timed out; status: {}", app.status.text);
            frame(ctx, app, vec![]);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn sample(dir: &Path, n: usize) -> PathBuf {
        let mut xml = String::from("<?xml version=\"1.0\"?>\n<catalog><parts>\n");
        for i in 0..n {
            xml.push_str(&format!("<part id=\"{i:05}\"><name>Bolt {i}</name><price cur=\"EUR\">{i}.5</price><price cur=\"USD\">{i}.9</price></part>\n"));
        }
        xml.push_str("</parts></catalog>\n");
        let path = dir.join("sample.xml");
        std::fs::write(&path, xml).unwrap();
        path
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tome-gui-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn open_browse_search_convert() {
        let dir = temp_dir("flow");
        let path = sample(&dir, 5000);
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.tag = "wrong".into(); // typed by the user: must not be replaced
        app.open(path.clone(), &ctx);
        wait(&ctx, &mut app, |a| a.open_job.is_none() && a.index_job.is_none() && a.text_job.is_none());
        assert_eq!(app.lines, Some(5003));
        assert_eq!(app.tag, "wrong");
        assert_eq!(app.ranges.as_ref().unwrap().len(), 0);
        assert!(app.status.text.contains("No <wrong> elements found"), "{}", app.status.text);
        assert!(app.status.text.contains("<part>"), "{}", app.status.text);

        // Fix the tag: re-index after the debounce.
        app.tag = "part".into();
        app.tag_edited_at = Some(Instant::now() - Duration::from_secs(1));
        wait(&ctx, &mut app, |a| a.index_job.is_none() && a.ranges.as_ref().is_some_and(|r| !r.is_empty()));
        assert_eq!(app.ranges.as_ref().unwrap().len(), 5000);

        // Expand a record and check the rows.
        app.toggle_record(2);
        let rows = &app.expanded[&2].rows;
        let keys: Vec<_> = rows.iter().map(|r| (r.key.as_str(), r.value.as_str())).collect();
        assert_eq!(keys[0], ("id", "00002"));
        assert_eq!(keys[1], ("name", "Bolt 2"));
        assert_eq!(keys[2], ("price [1]", ""));
        assert!(keys.contains(&("cur", "USD")));
        let segs = app.segments();
        assert_eq!(app.total_rows(&segs), 5000 + rows.len());
        assert_eq!(app.locate(2, &segs), (2, None));
        assert_eq!(app.locate(3, &segs), (2, Some(0)));
        assert_eq!(app.locate(3 + rows.len(), &segs), (3, None));
        frame(&ctx, &mut app, vec![]);

        // Select shows exact JSON.
        app.select(2);
        let json = &app.selected_json.as_ref().unwrap().1;
        assert!(json.starts_with("{\n  \"id\": \"00002\",\n  \"name\": [\n    {\n      \"#text\": \"Bolt 2\""), "{json}");
        frame(&ctx, &mut app, vec![]);

        // Search.
        app.search = "bolt 49".into();
        app.start_search(&ctx);
        wait(&ctx, &mut app, |a| a.search_job.is_none());
        let hits = app.hits.as_ref().unwrap();
        assert_eq!(hits.len(), 111, "Bolt 49, 490-499 and 4900-4999");
        assert_eq!(hits[0], 49);
        frame(&ctx, &mut app, vec![]);

        // Keyboard scrolling stays in range.
        for _ in 0..50 {
            frame(&ctx, &mut app, vec![egui::Event::Key { key: Key::PageDown, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }]);
        }
        assert!(app.top <= app.total_rows(&app.segments()));

        // Convert, then refuse to overwrite silently.
        app.output = dir.join("out.json").display().to_string();
        app.start_convert(&ctx, false);
        wait(&ctx, &mut app, |a| a.convert_job.is_none());
        assert!(app.status.text.starts_with("Wrote 5,000 records"), "{}", app.status.text);
        let written = std::fs::read_to_string(dir.join("out.json")).unwrap();
        assert!(written.starts_with("[\n  {\n    \"id\": \"00000\""));
        app.start_convert(&ctx, false);
        assert!(app.confirm_overwrite);
        assert!(app.convert_job.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cancel_leaves_no_files() {
        let dir = temp_dir("cancel");
        let path = sample(&dir, 200_000);
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.open(path, &ctx);
        wait(&ctx, &mut app, |a| a.ranges.is_some());
        app.output = dir.join("out.json").display().to_string();
        app.start_convert(&ctx, false);
        app.convert_job.as_ref().unwrap().cancel();
        wait(&ctx, &mut app, |a| a.convert_job.is_none());
        assert!(!dir.join("out.json").exists());
        assert!(!dir.join("out.json.tmp").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn broken_file_reports_error() {
        let dir = temp_dir("broken");
        let path = dir.join("broken.xml");
        std::fs::write(&path, "<r><part>\n<a></b></part></r>").unwrap();
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.open(path, &ctx);
        wait(&ctx, &mut app, |a| a.open_job.is_none() && a.index_job.is_none());
        assert!(app.status.error);
        assert!(app.status.text.contains("mismatched tag: line 2"), "{}", app.status.text);
        frame(&ctx, &mut app, vec![]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn plain_text_file() {
        let dir = temp_dir("text");
        let path = dir.join("notes.log");
        let mut content = String::new();
        for i in 0..50_000 {
            content.push_str(&format!("{i}\tline with\ttabs and a word{}\r\n", if i % 1000 == 7 { " NEEDLE" } else { "" }));
        }
        std::fs::write(&path, content).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.open(path, &ctx);
        wait(&ctx, &mut app, |a| a.text_job.is_none());
        assert!(app.kind.is_none());
        assert!(app.open_job.is_none(), "no XML parsing for a text file");
        assert_eq!(app.view, View::Text);
        assert_eq!(app.lines, Some(50_000));
        let text = app.text.clone().unwrap();
        assert!(text.crlf);
        frame(&ctx, &mut app, vec![]);

        // Search jumps to the first match and selects its line.
        app.textview.search = "needle".into();
        app.textview.start_search(&ctx, &text);
        wait(&ctx, &mut app, |a| !a.textview.busy());
        frame(&ctx, &mut app, vec![]);
        assert_eq!(app.textview.matches.as_ref().unwrap().len(), 50);
        assert_eq!(app.textview.selection, Some((7, 7)));
        assert!(app.status.text.starts_with("50 matches"), "{}", app.status.text);

        // Ctrl+C copies the selected line, without the CRLF.
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1200.0, 800.0))),
                events: vec![egui::Event::Copy],
                ..Default::default()
            },
            |ui| app.draw(ui),
        );
        assert_eq!(app.status.text, "Copied line 8.");

        // End scrolls to the last page; nothing panics past the end.
        frame(&ctx, &mut app, vec![egui::Event::Key { key: Key::End, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }]);
        assert_eq!(app.textview.selection, Some((49_999, 49_999)));
        for _ in 0..3 {
            frame(&ctx, &mut app, vec![egui::Event::Key { key: Key::PageDown, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }]);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn json_records_options_and_formats() {
        let dir = temp_dir("json");
        let path = dir.join("data.json");
        let mut items = Vec::new();
        for i in 0..3000 {
            items.push(format!(r#"{{"id":"{i:04}","name":"  Bolt {i} ","note":"","tags":["a"],"n":{i}.5}}"#));
        }
        std::fs::write(&path, format!(r#"{{"meta":{{"v":1}},"items":[{}]}}"#, items.join(","))).unwrap();
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.open(path.clone(), &ctx);
        wait(&ctx, &mut app, |a| a.open_job.is_none() && a.index_job.is_none() && a.text_job.is_none());
        assert_eq!(app.kind, Some(Kind::Json));
        assert_eq!(app.tag, "items", "the biggest array is picked and shown");
        assert_eq!(app.ranges.as_ref().unwrap().len(), 3000);
        assert_eq!(app.view, View::Records);
        assert!(app.output.ends_with("data-converted.json"), "{}", app.output);

        app.toggle_record(1);
        let rows: Vec<_> = app.expanded[&1].rows.iter().map(|r| (r.key.clone(), r.value.clone())).collect();
        assert_eq!(rows[0], ("id".to_string(), "0001".to_string()));
        assert!(rows.contains(&("tags".to_string(), "a".to_string())));
        assert!(rows.contains(&("n".to_string(), "1.5".to_string())));
        app.select(1);
        frame(&ctx, &mut app, vec![]);

        // Options change the panel and the tree right away.
        app.options.trim = true;
        app.options.remove_empty = true;
        app.options.unwrap_single = true;
        app.options_changed(&ctx);
        let json = &app.selected_json.as_ref().unwrap().1;
        assert_eq!(json, "{\n  \"id\": \"0001\",\n  \"name\": \"Bolt 1\",\n  \"tags\": \"a\",\n  \"n\": 1.5\n}");
        assert!(app.expanded.contains_key(&1), "expanded records stay open");

        // Convert to CSV with the options.
        app.format = Format::Csv;
        app.output = dir.join("out.csv").display().to_string();
        app.start_convert(&ctx, false);
        wait(&ctx, &mut app, |a| a.convert_job.is_none());
        assert!(app.status.text.starts_with("Wrote 3,000 records"), "{}", app.status.text);
        let csv = std::fs::read_to_string(dir.join("out.csv")).unwrap();
        let mut lines = csv.lines();
        assert_eq!(lines.next(), Some("id,name,tags,n"));
        assert_eq!(lines.next(), Some("0000,Bolt 0,a,0.5"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Run with: TOME_BIG_TEXT=/path/file cargo test --release big_text -- --ignored --nocapture
    #[test]
    #[ignore]
    fn big_text_frames() {
        let Ok(path) = std::env::var("TOME_BIG_TEXT") else { return };
        let ctx = egui::Context::default();
        let mut app = App::default();
        let t = Instant::now();
        app.open(PathBuf::from(path), &ctx);
        wait(&ctx, &mut app, |a| a.text_job.is_none());
        println!("open + index lines: {:.3}s  lines={:?}", t.elapsed().as_secs_f64(), app.lines);
        app.view = View::Text;
        let t = Instant::now();
        for _ in 0..20 { frame(&ctx, &mut app, vec![]); }
        println!("draw frame: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0 / 20.0);
        let text = app.text.clone().unwrap();
        app.textview.search = "1".into();
        let t = Instant::now();
        app.textview.start_search(&ctx, &text);
        wait(&ctx, &mut app, |a| !a.textview.busy());
        println!("search \"1\": {:.3}s  matches={}", t.elapsed().as_secs_f64(), app.textview.matches.as_ref().unwrap().len());
        let t = Instant::now();
        for _ in 0..20 { frame(&ctx, &mut app, vec![]); }
        println!("draw frame with highlights: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0 / 20.0);
    }

    /// Run with: TOME_BIG=/path/big.xml cargo test --release -- --ignored --nocapture
    #[test]
    #[ignore]
    fn big_file_timings() {
        let Ok(path) = std::env::var("TOME_BIG") else { return };
        let ctx = egui::Context::default();
        let mut app = App::default();
        let t = Instant::now();
        app.open(PathBuf::from(path), &ctx);
        wait(&ctx, &mut app, |a| a.open_job.is_none());
        println!("open + count lines + guess tag: {:.3}s  lines={:?} tag={}", t.elapsed().as_secs_f64(), app.lines, app.tag);
        let t = Instant::now();
        wait(&ctx, &mut app, |a| a.index_job.is_none());
        println!("index: {:.3}s  records={}", t.elapsed().as_secs_f64(), app.ranges.as_ref().unwrap().len());
        let t = Instant::now();
        for _ in 0..20 { frame(&ctx, &mut app, vec![]); }
        println!("draw frame: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0 / 20.0);
        app.goto = "2623582".into();
        let last = app.ranges.as_ref().unwrap().len();
        let segs = app.segments();
        app.top = app.flat_row_of(last - 1, &segs);
        let t = Instant::now();
        for _ in 0..20 { frame(&ctx, &mut app, vec![]); }
        println!("draw frame at end: {:.2} ms  top={}", t.elapsed().as_secs_f64() * 1000.0 / 20.0, app.top);
        app.search = "Supplier 42".into();
        let t = Instant::now();
        app.start_search(&ctx);
        wait(&ctx, &mut app, |a| a.search_job.is_none());
        println!("search: {:.3}s  hits={}", t.elapsed().as_secs_f64(), app.hits.as_ref().unwrap().len());
        app.search = "part number 1234".into();
        let t = Instant::now();
        app.start_search(&ctx);
        wait(&ctx, &mut app, |a| a.search_job.is_none());
        println!("search (case-insensitive): {:.3}s  hits={}", t.elapsed().as_secs_f64(), app.hits.as_ref().unwrap().len());
    }
}
