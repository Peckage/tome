//! The text view: any file, any size, one row per line, editable.
//!
//! Only the visible lines are decoded and drawn, and each one only as far as
//! the window is wide, so a 500 MB single-line file scrolls as smoothly as a
//! small one. Edits live in a piece table ([`crate::edit::Edits`]) over the
//! mapped file; search runs on all cores in the background and follows the
//! edits.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use eframe::egui::text::{CCursor, CCursorRange};
use eframe::egui::{self, Align2, Color32, FontId, Key, Modifiers, Rect, RichText, Sense, Stroke, Vec2};

use super::{Job, ROW_HEIGHT, scrollbar};
use crate::cli::group;
use crate::edit::{self, Edits, Hit};
use crate::engine::{Error, Result};
use crate::text::{self, Pattern, TextFile};

/// Keep at most this many matches.
const MATCH_LIMIT: usize = 10_000_000;
/// Copying more than this to the clipboard is cut short.
const COPY_LIMIT: usize = 64 << 20;
/// Lines longer than this are not edited in place.
const EDIT_LIMIT: usize = 1 << 20;
/// "Replace all" changes at most this many lines in one go.
const REPLACE_LIMIT: usize = 1_000_000;
/// Undo steps kept.
const UNDO_LIMIT: usize = 1000;

struct Found {
    needle: String,
    regex: bool,
    pattern: Option<Arc<Pattern>>,
    matches: Result<(Vec<Hit>, bool)>,
    seconds: f64,
    quiet: bool,
}

/// The line being edited in place.
struct LineEdit {
    line: usize,
    text: String,
    original: String,
    /// Put the cursor here (in characters) on the next frame.
    set_cursor: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveRequest {
    Save,
    SaveAs,
}

pub struct TextView {
    pub top: usize,
    pub left: usize,
    scroll_remainder: Vec2,
    /// (anchor line, cursor line) of the selected lines.
    pub selection: Option<(usize, usize)>,
    dragging: bool,

    pub search: String,
    pub regex: bool,
    searched: (String, bool),
    pattern: Option<Arc<Pattern>>,
    pub matches: Option<Arc<Vec<Hit>>>,
    truncated: bool,
    pub current: Option<usize>,
    search_job: Option<Job<Found>>,
    pub replace: String,
    pub show_replace: bool,

    pub goto: String,
    focus_search: bool,
    focus_goto: bool,
    /// Messages for the window's status bar: (text, is_error).
    pub status: Option<(String, bool)>,

    /// The document as edited, and as last opened or saved.
    pub edits: Edits,
    saved: Edits,
    undo: Vec<Edits>,
    redo: Vec<Edits>,
    editing: Option<LineEdit>,
    /// Why this file cannot be edited, if it cannot.
    pub read_only: Option<String>,
    /// Which file `edits` belongs to.
    file_id: usize,
    /// Set by Ctrl+S / Ctrl+Shift+S and the Save buttons; the window acts on it.
    pub save_request: Option<SaveRequest>,
}

impl Default for TextView {
    fn default() -> Self {
        TextView {
            top: 0,
            left: 0,
            scroll_remainder: Vec2::ZERO,
            selection: None,
            dragging: false,
            search: String::new(),
            regex: false,
            searched: (String::new(), false),
            pattern: None,
            matches: None,
            truncated: false,
            current: None,
            search_job: None,
            replace: String::new(),
            show_replace: false,
            goto: String::new(),
            focus_search: false,
            focus_goto: false,
            status: None,
            edits: Edits::new(0),
            saved: Edits::new(0),
            undo: Vec::new(),
            redo: Vec::new(),
            editing: None,
            read_only: None,
            file_id: 0,
            save_request: None,
        }
    }
}

impl TextView {
    /// A new file: forget everything but the search settings.
    pub fn reset(&mut self) {
        let (search, regex, replace, show_replace) = (std::mem::take(&mut self.search), self.regex, std::mem::take(&mut self.replace), self.show_replace);
        *self = TextView { search, regex, replace, show_replace, ..TextView::default() };
    }

    pub fn busy(&self) -> bool {
        self.search_job.is_some()
    }

    /// There are edits that are not saved.
    pub fn dirty(&self) -> bool {
        self.edits != self.saved || self.editing.as_ref().is_some_and(|e| e.text != e.original)
    }

    /// Commit the line being edited (before saving, switching views…).
    pub fn commit(&mut self) {
        self.commit_edit();
    }

    /// Adopt `file` if it is not the one the edits belong to.
    fn attach(&mut self, file: &Arc<TextFile>) {
        let id = Arc::as_ptr(file) as usize;
        if id == self.file_id {
            return;
        }
        self.file_id = id;
        self.edits = Edits::new(file.line_count());
        self.saved = self.edits.clone();
        self.undo.clear();
        self.redo.clear();
        self.editing = None;
        self.read_only = match file.encoding {
            "UTF-8" => None,
            other => Some(format!("{other} file: read-only (only UTF-8 text can be edited)")),
        };
        if let Some((a, b)) = self.selection {
            let last = file.line_count().saturating_sub(1);
            self.selection = Some((a.min(last), b.min(last)));
        }
    }

    // -----------------------------------------------------------------------
    // Editing
    // -----------------------------------------------------------------------

    fn change(&mut self, ctx: &egui::Context, file: &Arc<TextFile>, f: impl FnOnce(&mut Edits)) {
        self.undo.push(self.edits.clone());
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
        f(&mut self.edits);
        self.after_change(ctx, file);
    }

    fn after_change(&mut self, ctx: &egui::Context, file: &Arc<TextFile>) {
        let total = self.edits.line_count();
        if let Some((a, b)) = self.selection {
            let last = total.saturating_sub(1);
            self.selection = if total == 0 { None } else { Some((a.min(last), b.min(last))) };
        }
        // Hits moved with the lines: search again (it takes milliseconds).
        if !self.searched.0.is_empty() {
            self.run_search(ctx, file, true);
        }
    }

    fn undo(&mut self, ctx: &egui::Context, file: &Arc<TextFile>) {
        self.cancel_edit();
        if let Some(previous) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.edits, previous));
            self.after_change(ctx, file);
            self.status = Some(("Undone.".into(), false));
        }
    }

    fn redo(&mut self, ctx: &egui::Context, file: &Arc<TextFile>) {
        self.cancel_edit();
        if let Some(next) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.edits, next));
            self.after_change(ctx, file);
            self.status = Some(("Redone.".into(), false));
        }
    }

    fn line_string(&self, file: &TextFile, line: usize) -> std::result::Result<String, String> {
        let bytes = self.edits.line(file, line);
        if bytes.len() > EDIT_LIMIT {
            return Err(format!("Line {} is {} MB, too long to edit here.", group(line as u64 + 1), bytes.len() >> 20));
        }
        String::from_utf8(bytes.into_owned()).map_err(|_| format!("Line {} is not valid UTF-8 and cannot be edited.", group(line as u64 + 1)))
    }

    fn begin_edit(&mut self, file: &TextFile, line: usize, cursor: Option<usize>, visible: usize) -> bool {
        if let Some(reason) = &self.read_only {
            self.status = Some((reason.clone(), true));
            return false;
        }
        if line >= self.edits.line_count() {
            return false;
        }
        match self.line_string(file, line) {
            Ok(text) => {
                let at = cursor.unwrap_or(usize::MAX).min(text.chars().count());
                self.editing = Some(LineEdit { line, original: text.clone(), text, set_cursor: Some(at) });
                self.selection = Some((line, line));
                if line < self.top {
                    self.top = line;
                } else if line >= self.top + visible {
                    self.top = line + 1 - visible;
                }
                true
            }
            Err(e) => {
                self.status = Some((e, true));
                false
            }
        }
    }

    /// Store the line being edited; true when it changed.
    fn commit_edit(&mut self) -> bool {
        let Some(ed) = self.editing.take() else { return false };
        if ed.text == ed.original {
            return false;
        }
        self.undo.push(self.edits.clone());
        self.redo.clear();
        self.edits.set_line(ed.line, ed.text);
        true
    }

    fn cancel_edit(&mut self) {
        self.editing = None;
    }

    fn selected_range(&self) -> Option<(usize, usize)> {
        self.selection.map(|(a, b)| (a.min(b), a.max(b)))
    }

    fn delete_selection(&mut self, ctx: &egui::Context, file: &Arc<TextFile>) {
        let Some((first, last)) = self.selected_range() else { return };
        if self.edits.line_count() == 0 || self.read_only.is_some() {
            return;
        }
        self.change(ctx, file, |e| e.splice(first, last + 1, Vec::new()));
        let total = self.edits.line_count();
        self.selection = (total > 0).then(|| (first.min(total - 1), first.min(total - 1)));
        let n = last - first + 1;
        self.status = Some((if n == 1 { "Deleted a line.".into() } else { format!("Deleted {} lines.", group(n as u64)) }, false));
    }

    fn paste(&mut self, ctx: &egui::Context, file: &Arc<TextFile>, text: &str) {
        if self.read_only.is_some() {
            return;
        }
        let mut lines: Vec<String> = text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l).to_string()).collect();
        if lines.len() > 1 && lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        let at = self.selected_range().map_or(self.top, |(_, last)| last + 1).min(self.edits.line_count());
        let n = lines.len();
        self.change(ctx, file, |e| e.splice(at, at, lines));
        self.selection = Some((at, at + n - 1));
        self.status = Some((format!("Pasted {} line{} below.", group(n as u64), if n == 1 { "" } else { "s" }), false));
    }

    // -----------------------------------------------------------------------
    // Search and replace
    // -----------------------------------------------------------------------

    pub fn start_search(&mut self, ctx: &egui::Context, file: &Arc<TextFile>) {
        self.run_search(ctx, file, false);
    }

    fn run_search(&mut self, ctx: &egui::Context, file: &Arc<TextFile>, quiet: bool) {
        if let Some(job) = &self.search_job {
            job.cancel();
        }
        let (needle, regex) = if quiet { self.searched.clone() } else { (self.search.clone(), self.regex) };
        if needle.is_empty() {
            self.clear_search();
            return;
        }
        let file = file.clone();
        let edits = self.edits.clone();
        self.search_job = Some(Job::spawn(ctx, move |progress| {
            let started = Instant::now();
            let (pattern, matches) = match Pattern::new(&needle, regex) {
                Ok(p) => {
                    let p = Arc::new(p);
                    let m = edit::find(&file, &edits, &p, MATCH_LIMIT, progress);
                    (Some(p), m)
                }
                Err(e) => (None, Err(e)),
            };
            Found { needle, regex, pattern, matches, seconds: started.elapsed().as_secs_f64(), quiet }
        }));
    }

    fn clear_search(&mut self) {
        self.searched = (String::new(), false);
        self.pattern = None;
        self.matches = None;
        self.current = None;
        self.truncated = false;
    }

    fn poll(&mut self, file: &TextFile, visible: usize) {
        let Some(done) = self.search_job.as_ref().and_then(Job::poll) else { return };
        self.search_job = None;
        match done.matches {
            Ok((matches, truncated)) => {
                if !done.quiet {
                    let more = if truncated { "+" } else { "" };
                    let what = if done.regex { "matches of the pattern" } else { "matches" };
                    self.status = Some((
                        format!("{}{more} {what} \"{}\" ({:.2} s).", group(matches.len() as u64), done.needle, done.seconds),
                        false,
                    ));
                }
                self.searched = (done.needle, done.regex);
                self.pattern = done.pattern;
                self.truncated = truncated;
                let n = matches.len();
                let first = matches.partition_point(|m| m.line < self.top);
                let previous = self.current;
                self.matches = Some(Arc::new(matches));
                self.current = None;
                if done.quiet {
                    // After an edit: stay where we are, keep the index if it still exists.
                    self.current = previous.filter(|&i| i < n);
                } else if n > 0 {
                    self.jump_to_match(file, if first < n { first } else { 0 }, visible);
                }
            }
            Err(Error::Cancelled) => {}
            Err(e) => self.status = Some((format!("{e}"), true)),
        }
    }

    fn jump_to_match(&mut self, file: &TextFile, index: usize, visible: usize) {
        let Some(matches) = &self.matches else { return };
        let Some(&hit) = matches.get(index) else { return };
        self.current = Some(index);
        let line = hit.line;
        if line < self.top || line >= self.top + visible.saturating_sub(1) {
            self.top = line.saturating_sub(visible / 3);
        }
        self.selection = Some((line, line));
        let col = column_of(&self.edits.line(file, line), hit.start as usize);
        if col < self.left || col > self.left + 60 {
            self.left = col.saturating_sub(20);
        }
    }

    fn step(&mut self, file: &TextFile, forward: bool, visible: usize) {
        let Some(matches) = &self.matches else { return };
        let n = matches.len();
        if n == 0 {
            return;
        }
        let next = match self.current {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None => {
                let i = matches.partition_point(|m| m.line < self.top);
                if forward { i % n } else { (i + n - 1) % n }
            }
        };
        self.jump_to_match(file, next, visible);
    }

    /// Replace the current match, then show the next one.
    fn replace_one(&mut self, ctx: &egui::Context, file: &Arc<TextFile>, visible: usize) {
        if let Some(reason) = self.read_only.clone() {
            self.status = Some((reason, true));
            return;
        }
        let (Some(matches), Some(pattern)) = (self.matches.clone(), self.pattern.clone()) else { return };
        let Some(index) = self.current.or_else(|| (!matches.is_empty()).then_some(0)) else { return };
        let hit = matches[index];
        let line = self.edits.line(file, hit.line).into_owned();
        let replaced = pattern.replace_at(&line, hit.start as usize, &self.replace, self.searched.1);
        match String::from_utf8(replaced) {
            Ok(text) => {
                self.change(ctx, file, |e| e.set_line(hit.line, text));
                self.status = Some((format!("Replaced on line {}.", group(hit.line as u64 + 1)), false));
                self.jump_to_match(file, index, visible);
            }
            Err(_) => self.status = Some(("That line is not valid UTF-8.".into(), true)),
        }
    }

    fn replace_all(&mut self, ctx: &egui::Context, file: &Arc<TextFile>) {
        if let Some(reason) = self.read_only.clone() {
            self.status = Some((reason, true));
            return;
        }
        let (Some(matches), Some(pattern)) = (self.matches.clone(), self.pattern.clone()) else { return };
        let mut lines: Vec<usize> = matches.iter().map(|h| h.line).collect();
        lines.dedup();
        if lines.len() > REPLACE_LIMIT {
            self.status = Some((format!("{} lines match; Replace all handles up to {} at a time.", group(lines.len() as u64), group(REPLACE_LIMIT as u64)), true));
            return;
        }
        let expand = self.searched.1;
        let mut new_lines = Vec::with_capacity(lines.len());
        for &l in &lines {
            let replaced = pattern.replace_all(&self.edits.line(file, l), &self.replace, expand);
            match String::from_utf8(replaced) {
                Ok(t) => new_lines.push((l, t)),
                Err(_) => {
                    self.status = Some((format!("Line {} is not valid UTF-8; nothing was replaced.", group(l as u64 + 1)), true));
                    return;
                }
            }
        }
        let count = matches.len();
        let truncated = self.truncated;
        let line_count = lines.len();
        self.change(ctx, file, |e| {
            for (l, t) in new_lines {
                e.set_line(l, t);
            }
        });
        let more = if truncated { " (the first ones; search again for more)" } else { "" };
        self.status = Some((format!("Replaced {} matches on {} lines{more}.", group(count as u64), group(line_count as u64)), false));
    }

    #[cfg(test)]
    pub fn replace_all_for_test(&mut self, ctx: &egui::Context, file: &Arc<TextFile>) {
        self.replace_all(ctx, file);
    }

    fn copy_selection(&mut self, ctx: &egui::Context, file: &TextFile) {
        let Some((first, last)) = self.selected_range() else { return };
        let (text, cut) = self.edits.copy_lines(file, first, last, COPY_LIMIT);
        ctx.copy_text(text);
        let lines = last - first + 1;
        self.status = Some(if cut {
            (format!("Copied the first {} MB of {} lines (clipboard limit).", COPY_LIMIT >> 20, group(lines as u64)), false)
        } else if lines == 1 {
            (format!("Copied line {}.", group(first as u64 + 1)), false)
        } else {
            (format!("Copied {} lines.", group(lines as u64)), false)
        });
    }

    // -----------------------------------------------------------------------
    // Drawing
    // -----------------------------------------------------------------------

    pub fn draw(&mut self, ui: &mut egui::Ui, file: &Arc<TextFile>) {
        self.attach(file);
        let ctx = ui.ctx().clone();
        let rows_guess = ((ui.available_height() - 40.0) / ROW_HEIGHT).max(1.0) as usize;
        self.poll(file, rows_guess);

        // Shortcuts that work anywhere in the window.
        let (ctrl_f, ctrl_g, ctrl_h, f3, shift, save, save_as) = ui.input(|i| {
            let cmd = i.modifiers.command;
            (
                cmd && i.key_pressed(Key::F),
                cmd && i.key_pressed(Key::G),
                cmd && i.key_pressed(Key::H),
                i.key_pressed(Key::F3),
                i.modifiers.shift,
                cmd && !i.modifiers.shift && i.key_pressed(Key::S),
                cmd && i.modifiers.shift && i.key_pressed(Key::S),
            )
        });
        if ctrl_f {
            self.focus_search = true;
        }
        if ctrl_h {
            self.show_replace = !self.show_replace;
            self.focus_search = true;
        }
        if ctrl_g {
            self.focus_goto = true;
        }
        if f3 {
            self.step(file, !shift, rows_guess);
        }
        if save || save_as {
            self.commit_edit();
            self.save_request = Some(if save_as { SaveRequest::SaveAs } else { SaveRequest::Save });
        }

        self.toolbar(ui, file, rows_guess, shift);
        if self.edits.line_count() == 0 {
            ui.centered_and_justified(|ui| {
                let msg = if file.line_count() == 0 { "This file is empty. Ctrl+Enter adds a line." } else { "Every line was deleted. Ctrl+Z undoes it." };
                ui.label(RichText::new(msg).weak().size(16.0))
            });
            let (ctrl_enter, undo) = ui.input(|i| (i.modifiers.command && i.key_pressed(Key::Enter), i.modifiers.command && i.key_pressed(Key::Z)));
            if undo {
                self.undo(&ctx, file);
            } else if ctrl_enter && self.read_only.is_none() {
                self.change(&ctx, file, |e| e.splice(0, 0, vec![String::new()]));
                self.begin_edit(file, 0, Some(0), rows_guess);
            }
            return;
        }
        self.lines(ui, file, shift);
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, file: &Arc<TextFile>, rows_guess: usize, shift: bool) {
        let ctx = ui.ctx().clone();
        let total = self.edits.line_count();
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Find (Ctrl+F). Case-insensitive unless you type capitals.")
                    .desired_width((ui.available_width() - 560.0).max(160.0)),
            );
            if std::mem::take(&mut self.focus_search) {
                field.request_focus();
            }
            if field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                if (self.search.clone(), self.regex) == self.searched && self.matches.is_some() {
                    self.step(file, !shift, rows_guess);
                } else {
                    self.start_search(&ctx, file);
                }
                field.request_focus();
            }
            ui.toggle_value(&mut self.regex, ".*").on_hover_text("Regular expression");
            if self.search_job.is_some() {
                if ui.button("Stop").clicked() {
                    if let Some(job) = &self.search_job {
                        job.cancel();
                    }
                }
            } else if ui.button("Find").clicked() {
                self.start_search(&ctx, file);
            }
            let has = self.matches.as_ref().is_some_and(|m| !m.is_empty());
            if ui.add_enabled(has, egui::Button::new("◀")).on_hover_text("Previous match (Shift+F3)").clicked() {
                self.step(file, false, rows_guess);
            }
            if ui.add_enabled(has, egui::Button::new("▶")).on_hover_text("Next match (F3)").clicked() {
                self.step(file, true, rows_guess);
            }
            if let Some(m) = &self.matches {
                let more = if self.truncated { "+" } else { "" };
                let label = match self.current {
                    Some(i) => format!("{} / {}{more}", group(i as u64 + 1), group(m.len() as u64)),
                    None => format!("{}{more} matches", group(m.len() as u64)),
                };
                ui.label(RichText::new(label).weak());
                if ui.small_button("✕").on_hover_text("Clear search").clicked() {
                    self.clear_search();
                }
            }
            ui.toggle_value(&mut self.show_replace, "Replace").on_hover_text("Find and replace (Ctrl+H)");
            ui.separator();
            ui.label("Line");
            let goto = ui.add(egui::TextEdit::singleline(&mut self.goto).desired_width(80.0).hint_text("Ctrl+G"));
            if std::mem::take(&mut self.focus_goto) {
                goto.request_focus();
            }
            if goto.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                if let Ok(n) = self.goto.trim().replace([',', '_', ' '], "").parse::<usize>() {
                    if total > 0 {
                        let line = n.clamp(1, total) - 1;
                        self.top = line.saturating_sub(rows_guess / 3);
                        self.selection = Some((line, line));
                        self.left = 0;
                    }
                }
            }
            ui.separator();
            let endings = if file.crlf { "CRLF" } else { "LF" };
            ui.label(RichText::new(format!("{}  ·  {endings}", file.encoding)).weak());
            if let Some(reason) = &self.read_only {
                ui.label(RichText::new("🔒 read-only").weak()).on_hover_text(reason.as_str());
            } else if self.dirty() {
                ui.separator();
                ui.label(RichText::new("● unsaved").color(ui.visuals().warn_fg_color));
                if ui.button("Save").on_hover_text("Save the edits to this file (Ctrl+S)").clicked() {
                    self.commit_edit();
                    self.save_request = Some(SaveRequest::Save);
                }
            }
        });
        if self.show_replace {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.replace)
                        .hint_text(if self.regex { "Replace with ($1 = first group)" } else { "Replace with" })
                        .desired_width((ui.available_width() - 560.0).max(160.0)),
                );
                let ready = self.matches.as_ref().is_some_and(|m| !m.is_empty()) && (self.search.clone(), self.regex) == self.searched;
                if ui.add_enabled(ready, egui::Button::new("Replace")).on_hover_text("Replace this match").clicked() {
                    self.replace_one(&ctx, file, rows_guess);
                }
                if ui.add_enabled(ready, egui::Button::new("Replace all")).clicked() {
                    self.replace_all(&ctx, file);
                }
                if !ready {
                    ui.label(RichText::new("Find first, then replace.").weak());
                }
            });
        }
        if let Some(job) = &self.search_job {
            let done = job.progress.bytes.load(Ordering::Relaxed) as f32 / file.len().max(1) as f32;
            ui.add(egui::ProgressBar::new(done).text(format!("Searching… {:.0}%", done * 100.0)));
        }
        ui.add_space(4.0);
    }

    fn lines(&mut self, ui: &mut egui::Ui, file: &Arc<TextFile>, shift: bool) {
        let ctx = ui.ctx().clone();

        // Geometry.
        let area = ui.available_rect_before_wrap();
        let scrollbar_w = 12.0;
        let list = Rect::from_min_max(area.min, egui::pos2(area.max.x - scrollbar_w - 2.0, area.max.y));
        let bar = Rect::from_min_max(egui::pos2(area.max.x - scrollbar_w, area.min.y), area.max);
        let visible = ((list.height() / ROW_HEIGHT).floor() as usize).max(1);
        let mono = FontId::monospace(13.0);
        let char_w = ui.ctx().fonts_mut(|f| f.glyph_width(&mono, '0')).max(1.0);
        let digits = group(self.edits.line_count() as u64).len();
        let gutter = digits as f32 * char_w + 16.0;
        let text_x = list.min.x + gutter + 6.0;
        let columns = (((list.max.x - text_x) / char_w).floor() as usize).max(1);
        let max_left = (file.longest_line as usize).max(4096).saturating_sub(columns / 2);
        let edit_id = ui.make_persistent_id("tome-line-edit");

        // Mouse wheel (vertical and sideways; Shift+wheel scrolls sideways).
        let response = ui.interact(list, ui.scope_id().with("text-lines"), Sense::click_and_drag());
        if ui.rect_contains_pointer(list) {
            let max_top = self.edits.line_count().saturating_sub(visible);
            let (mut delta, shift_held) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers.shift));
            if shift_held && delta.x == 0.0 {
                delta = Vec2::new(delta.y, 0.0);
            }
            self.scroll_remainder -= delta;
            let rows = (self.scroll_remainder.y / ROW_HEIGHT).trunc();
            if rows != 0.0 {
                self.scroll_remainder.y -= rows * ROW_HEIGHT;
                self.top = (self.top as i64 + rows as i64).clamp(0, max_top as i64) as usize;
            }
            let cols = (self.scroll_remainder.x / char_w).trunc();
            if cols != 0.0 && self.editing.is_none() {
                self.scroll_remainder.x -= cols * char_w;
                self.left = (self.left as i64 + cols as i64).clamp(0, max_left as i64) as usize;
            }
        }

        // Keys for the line being edited, taken before the text field sees them.
        if self.editing.is_some() {
            self.edit_keys(&ctx, file, edit_id, visible);
        }

        // Keyboard, when no text field has focus.
        let typing = ui.ctx().memory(|m| m.focused().is_some());
        if !typing && self.editing.is_none() {
            self.view_keys(ui, &ctx, file, visible, max_left, shift);
        }
        let total = self.edits.line_count();
        if total == 0 {
            return;
        }
        let max_top = total.saturating_sub(visible);
        self.top = self.top.min(max_top);

        // Clicks and drags select lines; a double click edits.
        let top_now = self.top;
        let pointer_line = move |p: egui::Pos2| ((p.y - list.min.y) / ROW_HEIGHT).floor().max(0.0) as usize + top_now;
        if response.double_clicked() {
            if let Some(p) = response.interact_pointer_pos() {
                let line = pointer_line(p).min(total - 1);
                let col = ((p.x - text_x) / char_w).round().max(0.0) as usize + self.left;
                let char_index = char_at_column(&self.edits.line(file, line), col);
                if self.commit_edit() {
                    self.after_change(&ctx, file);
                }
                self.begin_edit(file, line, Some(char_index), visible);
            }
        } else if let Some(p) = response.interact_pointer_pos() {
            let line = pointer_line(p).min(total - 1);
            if response.drag_started() || response.clicked() {
                let anchor = match self.selection {
                    Some((a, _)) if shift => a,
                    _ => line,
                };
                self.selection = Some((anchor, line));
                self.dragging = response.drag_started();
            } else if response.dragged() && self.dragging {
                if let Some((a, _)) = self.selection {
                    self.selection = Some((a, line));
                }
                if p.y > list.max.y {
                    self.top = (self.top + 1).min(max_top);
                }
                if p.y < list.min.y {
                    self.top = self.top.saturating_sub(1);
                }
            }
        }
        if response.drag_stopped() {
            self.dragging = false;
        }
        if response.secondary_clicked() {
            if let Some(p) = response.interact_pointer_pos() {
                let line = pointer_line(p).min(total - 1);
                let inside = self.selected_range().is_some_and(|(a, b)| line >= a && line <= b);
                if !inside {
                    self.selection = Some((line, line));
                }
            }
        }
        self.context_menu(&response, &ctx, file, visible);
        let total = self.edits.line_count();
        if total == 0 {
            return;
        }
        self.top = self.top.min(total.saturating_sub(visible));

        // Draw.
        let painter = ui.painter_at(list);
        let visuals = ui.visuals().clone();
        let weak = visuals.weak_text_color();
        let edited_mark = if visuals.dark_mode { Color32::from_rgb(90, 160, 255) } else { Color32::from_rgb(30, 110, 220) };
        let hit_fill = if visuals.dark_mode { Color32::from_rgba_unmultiplied(230, 180, 40, 70) } else { Color32::from_rgba_unmultiplied(250, 200, 0, 110) };
        let current_fill = Color32::from_rgba_unmultiplied(255, 140, 0, 155);
        painter.rect_filled(Rect::from_min_max(list.min, egui::pos2(list.min.x + gutter, list.max.y)), 0.0, visuals.faint_bg_color);
        let sel = self.selected_range();
        let current_hit = self.current.and_then(|i| self.matches.as_ref().and_then(|m| m.get(i).copied()));
        let editing_line = self.editing.as_ref().map(|e| e.line);
        for row in 0..visible.min(total - self.top) {
            let line = self.top + row;
            let y = list.min.y + row as f32 * ROW_HEIGHT;
            let row_rect = Rect::from_min_size(egui::pos2(list.min.x, y), Vec2::new(list.width(), ROW_HEIGHT));
            if sel.is_some_and(|(a, b)| line >= a && line <= b) {
                painter.rect_filled(row_rect, 0.0, visuals.selection.bg_fill.gamma_multiply(0.45));
            }
            let mid = y + ROW_HEIGHT / 2.0;
            if self.edits.is_new(line) {
                painter.rect_filled(Rect::from_min_size(egui::pos2(list.min.x, y + 2.0), Vec2::new(3.0, ROW_HEIGHT - 4.0)), 1.0, edited_mark);
            }
            painter.text(egui::pos2(list.min.x + gutter - 8.0, mid), Align2::RIGHT_CENTER, (line + 1).to_string(), mono.clone(), weak);
            if editing_line == Some(line) {
                continue; // drawn as a text field below
            }
            let bytes = self.edits.line(file, line);

            // Highlight matches on this line. Columns are counted
            // incrementally, so a long line with many matches costs one pass.
            if let Some(matches) = &self.matches {
                let mut i = matches.partition_point(|m| m.line < line);
                let (mut at, mut col) = (0usize, 0usize);
                while let Some(&hit) = matches.get(i) {
                    if hit.line != line || hit.start as usize > (1 << 20) {
                        break;
                    }
                    let (s_off, e_off) = (hit.start as usize, hit.end as usize);
                    col = advance_column(&bytes, at, s_off.max(at), col);
                    at = s_off.max(at);
                    let c0 = col;
                    let c1 = advance_column(&bytes, at, e_off.max(at), col).max(c0 + 1);
                    if c0 >= self.left + columns {
                        break;
                    }
                    if c1 > self.left {
                        let x0 = text_x + c0.saturating_sub(self.left) as f32 * char_w;
                        let x1 = text_x + (c1 - self.left).min(columns) as f32 * char_w;
                        let fill = if current_hit == Some(hit) { current_fill } else { hit_fill };
                        painter.rect_filled(Rect::from_min_max(egui::pos2(x0, y + 1.0), egui::pos2(x1, y + ROW_HEIGHT - 1.0)), 2.0, fill);
                    }
                    i += 1;
                }
            }
            let shown = text::display_window(&bytes, self.left, columns + 1);
            painter.text(egui::pos2(text_x, mid), Align2::LEFT_CENTER, shown, mono.clone(), visuals.text_color());
        }

        // The line being edited.
        if let Some(line) = editing_line.filter(|l| *l >= self.top && *l < self.top + visible) {
            let y = list.min.y + (line - self.top) as f32 * ROW_HEIGHT;
            let rect = Rect::from_min_max(egui::pos2(text_x - 2.0, y), egui::pos2(list.max.x - 2.0, y + ROW_HEIGHT));
            painter.rect_filled(rect, 2.0, visuals.extreme_bg_color);
            painter.rect_stroke(rect, 2.0, Stroke::new(1.0, visuals.selection.stroke.color), egui::StrokeKind::Inside);
            let ed = self.editing.as_mut().unwrap();
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
            let output = egui::TextEdit::singleline(&mut ed.text)
                .id(edit_id)
                .font(mono.clone())
                .frame(egui::Frame::NONE)
                .margin(Vec2::new(2.0, 2.0))
                .desired_width(rect.width())
                .show(&mut child);
            if let Some(at) = ed.set_cursor.take() {
                let mut state = egui::TextEdit::load_state(&ctx, edit_id).unwrap_or_default();
                state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(at))));
                egui::TextEdit::store_state(&ctx, edit_id, state);
                output.response.request_focus();
            } else if output.response.lost_focus() {
                // Clicked elsewhere: keep what was typed.
                if self.commit_edit() {
                    self.after_change(&ctx, file);
                }
            }
        } else if self.editing.as_ref().is_some_and(|e| e.set_cursor.is_none()) && !ctx.memory(|m| m.has_focus(edit_id)) {
            // Scrolled away from the line: keep it.
            if self.commit_edit() {
                self.after_change(&ctx, file);
            }
        }

        if let Some(top) = scrollbar(ui, bar, self.top, total, visible, ui.scope_id().with("text-scrollbar")) {
            self.top = top;
        }
        painter.rect_stroke(list, 4.0, Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color), egui::StrokeKind::Outside);
        ui.allocate_rect(area, Sense::hover());
    }

    /// Enter, Escape, ↑/↓, Backspace at the start and Delete at the end of
    /// the line being edited.
    fn edit_keys(&mut self, ctx: &egui::Context, file: &Arc<TextFile>, id: egui::Id, visible: usize) {
        let Some(ed) = &self.editing else { return };
        let cursor = egui::TextEdit::load_state(ctx, id).and_then(|s| s.cursor.char_range());
        let len = ed.text.chars().count();
        let (at, collapsed) = match (cursor, ed.set_cursor) {
            (_, Some(at)) => (at, true),
            (Some(r), None) => (r.primary.index.0, r.primary.index.0 == r.secondary.index.0),
            (None, None) => (len, true),
        };
        let line = ed.line;
        let total = self.edits.line_count();
        let (enter, escape, up, down, backspace, delete) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Escape),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                collapsed && at == 0 && line > 0 && i.consume_key(Modifiers::NONE, Key::Backspace),
                collapsed && at == len && line + 1 < total && i.consume_key(Modifiers::NONE, Key::Delete),
            )
        });
        if escape {
            self.cancel_edit();
            return;
        }
        if enter {
            // Split the line at the cursor.
            let ed = self.editing.take().unwrap();
            let split = ed.text.char_indices().nth(at).map_or(ed.text.len(), |(b, _)| b);
            let (before, after) = (ed.text[..split].to_string(), ed.text[split..].to_string());
            self.change(ctx, file, |e| e.splice(line, line + 1, vec![before, after]));
            self.begin_edit(file, line + 1, Some(0), visible);
            return;
        }
        if up || down {
            if self.commit_edit() {
                self.after_change(ctx, file);
            }
            let target = if up { line.checked_sub(1) } else { (line + 1 < self.edits.line_count()).then_some(line + 1) };
            self.begin_edit(file, target.unwrap_or(line), Some(at), visible);
            return;
        }
        if backspace {
            match self.line_string(file, line - 1) {
                Ok(previous) => {
                    let ed = self.editing.take().unwrap();
                    let cursor = previous.chars().count();
                    let joined = previous + &ed.text;
                    self.change(ctx, file, |e| e.splice(line - 1, line + 1, vec![joined]));
                    self.begin_edit(file, line - 1, Some(cursor), visible);
                }
                Err(e) => self.status = Some((e, true)),
            }
            return;
        }
        if delete {
            match self.line_string(file, line + 1) {
                Ok(next) => {
                    let ed = self.editing.take().unwrap();
                    let joined = ed.text + &next;
                    self.change(ctx, file, |e| e.splice(line, line + 2, vec![joined]));
                    self.begin_edit(file, line, Some(at), visible);
                }
                Err(e) => self.status = Some((e, true)),
            }
        }
    }

    fn view_keys(&mut self, ui: &egui::Ui, ctx: &egui::Context, file: &Arc<TextFile>, visible: usize, max_left: usize, shift: bool) {
        struct Keys {
            down: bool,
            up: bool,
            pgdn: bool,
            pgup: bool,
            home: bool,
            end: bool,
            left: bool,
            right: bool,
            copy: bool,
            cut: bool,
            paste: Option<String>,
            select_all: bool,
            enter: bool,
            ctrl_enter: bool,
            delete: bool,
            undo: bool,
            redo: bool,
            duplicate: bool,
            typed: Option<String>,
        }
        let k = ui.input(|i| {
            let cmd = i.modifiers.command;
            Keys {
                down: i.key_pressed(Key::ArrowDown),
                up: i.key_pressed(Key::ArrowUp),
                pgdn: i.key_pressed(Key::PageDown),
                pgup: i.key_pressed(Key::PageUp),
                home: i.key_pressed(Key::Home),
                end: i.key_pressed(Key::End),
                left: i.key_pressed(Key::ArrowLeft),
                right: i.key_pressed(Key::ArrowRight),
                copy: i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
                cut: i.events.iter().any(|e| matches!(e, egui::Event::Cut)),
                paste: i.events.iter().find_map(|e| match e {
                    egui::Event::Paste(t) => Some(t.clone()),
                    _ => None,
                }),
                select_all: cmd && i.key_pressed(Key::A),
                enter: !cmd && (i.key_pressed(Key::Enter) || i.key_pressed(Key::F2)),
                ctrl_enter: cmd && i.key_pressed(Key::Enter),
                delete: i.key_pressed(Key::Delete),
                undo: cmd && !i.modifiers.shift && i.key_pressed(Key::Z),
                redo: cmd && (i.key_pressed(Key::Y) || (i.modifiers.shift && i.key_pressed(Key::Z))),
                duplicate: cmd && i.key_pressed(Key::D),
                typed: i.events.iter().find_map(|e| match e {
                    egui::Event::Text(t) if !cmd => Some(t.clone()),
                    _ => None,
                }),
            }
        });
        if k.undo {
            self.undo(ctx, file);
            return;
        }
        if k.redo {
            self.redo(ctx, file);
            return;
        }
        if let Some(text) = k.paste {
            self.paste(ctx, file, &text);
            return;
        }
        let total = self.edits.line_count();
        if total == 0 {
            return;
        }
        let max_top = total.saturating_sub(visible);
        let page = visible.saturating_sub(1).max(1);
        let mut moved = None;
        if let Some((_, cursor)) = self.selection {
            if k.down { moved = Some((cursor + 1).min(total - 1)); }
            if k.up { moved = Some(cursor.saturating_sub(1)); }
            if k.pgdn { moved = Some((cursor + page).min(total - 1)); }
            if k.pgup { moved = Some(cursor.saturating_sub(page)); }
        } else {
            if k.down { self.top = (self.top + 1).min(max_top); }
            if k.up { self.top = self.top.saturating_sub(1); }
            if k.pgdn { self.top = (self.top + page).min(max_top); }
            if k.pgup { self.top = self.top.saturating_sub(page); }
        }
        if k.home {
            moved = Some(0);
            self.left = 0;
        }
        if k.end {
            moved = Some(total - 1);
        }
        if let Some(line) = moved {
            let anchor = match self.selection {
                Some((a, _)) if shift => a,
                _ => line,
            };
            self.selection = Some((anchor, line));
            if line < self.top {
                self.top = line;
            } else if line >= self.top + visible {
                self.top = line + 1 - visible;
            }
        }
        if k.left { self.left = self.left.saturating_sub(4); }
        if k.right { self.left = (self.left + 4).min(max_left); }
        if k.select_all {
            self.selection = Some((0, total - 1));
        }
        if k.copy {
            self.copy_selection(ctx, file);
        }
        if k.cut {
            self.copy_selection(ctx, file);
            self.delete_selection(ctx, file);
            return;
        }
        if k.delete {
            self.delete_selection(ctx, file);
            return;
        }
        if k.duplicate {
            if let (Some((first, last)), None) = (self.selected_range(), &self.read_only) {
                let lines: Vec<String> = (first..=last).map(|l| String::from_utf8_lossy(&self.edits.line(file, l)).into_owned()).collect();
                let n = lines.len();
                self.change(ctx, file, |e| e.splice(last + 1, last + 1, lines));
                self.selection = Some((last + 1, last + n));
            }
            return;
        }
        if k.ctrl_enter {
            let at = self.selected_range().map_or(self.top, |(_, last)| last + 1);
            if self.read_only.is_none() {
                self.change(ctx, file, |e| e.splice(at, at, vec![String::new()]));
                self.begin_edit(file, at, Some(0), visible);
            }
            return;
        }
        if let Some((_, cursor)) = self.selection {
            if k.enter {
                self.begin_edit(file, cursor, None, visible);
            } else if let Some(t) = k.typed.filter(|t| !t.is_empty()) {
                // Typing on a selected line starts editing it, at its end.
                if self.begin_edit(file, cursor, None, visible) {
                    if let Some(ed) = &mut self.editing {
                        ed.text.push_str(&t);
                        ed.set_cursor = Some(ed.text.chars().count());
                    }
                }
            }
        }
    }

    fn context_menu(&mut self, response: &egui::Response, ctx: &egui::Context, file: &Arc<TextFile>, visible: usize) {
        let total = self.edits.line_count();
        let editable = self.read_only.is_none();
        response.context_menu(|ui| {
            let Some((first, last)) = self.selected_range() else {
                ui.close();
                return;
            };
            let n = last - first + 1;
            let lines = if n == 1 { "line".to_string() } else { format!("{} lines", group(n as u64)) };
            if ui.add_enabled(editable && n == 1, egui::Button::new("Edit line    Enter")).clicked() {
                self.begin_edit(file, first, None, visible);
                ui.close();
            }
            if ui.add_enabled(editable, egui::Button::new("Insert line below    Ctrl+Enter")).clicked() {
                self.change(ctx, file, |e| e.splice(last + 1, last + 1, vec![String::new()]));
                self.begin_edit(file, last + 1, Some(0), visible);
                ui.close();
            }
            if ui.add_enabled(editable, egui::Button::new(format!("Duplicate {lines}    Ctrl+D"))).clicked() {
                let copy: Vec<String> = (first..=last).map(|l| String::from_utf8_lossy(&self.edits.line(file, l)).into_owned()).collect();
                self.change(ctx, file, |e| e.splice(last + 1, last + 1, copy));
                self.selection = Some((last + 1, last + n));
                ui.close();
            }
            ui.separator();
            if ui.button(format!("Copy {lines}    Ctrl+C")).clicked() {
                self.copy_selection(ctx, file);
                ui.close();
            }
            if ui.add_enabled(editable, egui::Button::new(format!("Cut {lines}    Ctrl+X"))).clicked() {
                self.copy_selection(ctx, file);
                self.delete_selection(ctx, file);
                ui.close();
            }
            if ui.add_enabled(editable, egui::Button::new(format!("Delete {lines}    Del"))).clicked() {
                self.delete_selection(ctx, file);
                ui.close();
            }
            if ui.button("Copy line number").clicked() {
                ctx.copy_text((first + 1).to_string());
                ui.close();
            }
            ui.separator();
            if ui.add_enabled(!self.undo.is_empty(), egui::Button::new("Undo    Ctrl+Z")).clicked() {
                self.undo(ctx, file);
                ui.close();
            }
            if ui.add_enabled(!self.redo.is_empty(), egui::Button::new("Redo    Ctrl+Y")).clicked() {
                self.redo(ctx, file);
                ui.close();
            }
            if ui.button("Select all    Ctrl+A").clicked() {
                self.selection = Some((0, total.saturating_sub(1)));
                ui.close();
            }
        });
    }
}

/// Display column (tabs expanded, 4 wide) of byte `offset` in `line`.
fn column_of(line: &[u8], offset: usize) -> usize {
    advance_column(line, 0, offset, 0)
}

/// The character index nearest to display column `col` (to place the
/// cursor where the line was double-clicked).
fn char_at_column(line: &[u8], col: usize) -> usize {
    let text = String::from_utf8_lossy(&line[..line.len().min((col + 1) * 4 + 4)]);
    let mut c = 0usize;
    for (i, ch) in text.chars().enumerate() {
        let next = if ch == '\t' { c + 4 - c % 4 } else { c + 1 };
        if next > col {
            return i;
        }
        c = next;
    }
    text.chars().count()
}

/// The column at byte `to`, given that byte `from` is at column `col`.
fn advance_column(line: &[u8], from: usize, to: usize, mut col: usize) -> usize {
    let to = to.min(line.len());
    if from >= to {
        return col;
    }
    for &b in &line[from..to] {
        if b == b'\t' {
            col += 4 - col % 4;
        } else if b & 0xC0 != 0x80 {
            col += 1; // count characters, not UTF-8 continuation bytes
        }
    }
    col
}
