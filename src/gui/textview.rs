//! The raw text view: any file, any size, one row per line.
//!
//! Only the visible lines are decoded and drawn, and each one only as far as
//! the window is wide, so a 500 MB single-line file scrolls as smoothly as a
//! small one. Search runs on all cores in the background.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use eframe::egui::{self, Align2, Color32, FontId, Key, Rect, RichText, Sense, Stroke, Vec2};

use super::{Job, ROW_HEIGHT, scrollbar};
use crate::cli::group;
use crate::engine::{Error, Result};
use crate::text::{self, Pattern, TextFile};

/// Keep at most this many matches (16 bytes each).
const MATCH_LIMIT: usize = 10_000_000;
/// Copying more than this to the clipboard is cut short.
const COPY_LIMIT: usize = 64 << 20;
/// Highlight matches only this far into a line (column counting cost).
const HIGHLIGHT_REACH: usize = 1 << 20;

struct Found {
    needle: String,
    regex: bool,
    pattern: Option<Arc<Pattern>>,
    matches: Result<(Vec<(u64, u64)>, bool)>,
    seconds: f64,
}

pub struct TextView {
    pub top: usize,
    left: usize,
    scroll_remainder: Vec2,
    /// (anchor line, cursor line) of the selected lines.
    pub selection: Option<(usize, usize)>,
    dragging: bool,

    pub search: String,
    pub regex: bool,
    searched: (String, bool),
    pattern: Option<Arc<Pattern>>,
    pub matches: Option<Arc<Vec<(u64, u64)>>>,
    truncated: bool,
    pub current: Option<usize>,
    search_job: Option<Job<Found>>,

    pub goto: String,
    focus_search: bool,
    focus_goto: bool,
    /// Messages for the window's status bar: (text, is_error).
    pub status: Option<(String, bool)>,
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
            goto: String::new(),
            focus_search: false,
            focus_goto: false,
            status: None,
        }
    }
}

impl TextView {
    pub fn reset(&mut self) {
        let (search, regex) = (std::mem::take(&mut self.search), self.regex);
        *self = TextView { search, regex, ..TextView::default() };
    }

    pub fn busy(&self) -> bool {
        self.search_job.is_some()
    }

    pub fn start_search(&mut self, ctx: &egui::Context, file: &Arc<TextFile>) {
        if let Some(job) = &self.search_job {
            job.cancel();
        }
        let needle = self.search.clone();
        if needle.is_empty() {
            self.clear_search();
            return;
        }
        let regex = self.regex;
        let file = file.clone();
        self.search_job = Some(Job::spawn(ctx, move |progress| {
            let started = Instant::now();
            let (pattern, matches) = match Pattern::new(&needle, regex) {
                Ok(p) => {
                    let p = Arc::new(p);
                    let m = text::find_all(&file, &p, MATCH_LIMIT, progress);
                    (Some(p), m)
                }
                Err(e) => (None, Err(e)),
            };
            Found { needle, regex, pattern, matches, seconds: started.elapsed().as_secs_f64() }
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
                let more = if truncated { "+" } else { "" };
                let what = if done.regex { "matches of the pattern" } else { "matches" };
                self.status = Some((
                    format!("{}{more} {what} \"{}\" ({:.2} s).", group(matches.len() as u64), done.needle, done.seconds),
                    false,
                ));
                self.searched = (done.needle, done.regex);
                self.pattern = done.pattern;
                self.truncated = truncated;
                // Start at the first match at or after the current view.
                let from = if self.top < file.line_count() { file.line_start(self.top) } else { 0 };
                let first = matches.partition_point(|m| m.0 < from);
                let has = !matches.is_empty();
                self.matches = Some(Arc::new(matches));
                self.current = None;
                if has {
                    let n = self.matches.as_ref().unwrap().len();
                    self.jump_to_match(file, if first < n { first } else { 0 }, visible);
                }
            }
            Err(Error::Cancelled) => self.status = Some(("Search stopped.".into(), false)),
            Err(e) => self.status = Some((format!("{e}"), true)),
        }
    }

    fn jump_to_match(&mut self, file: &TextFile, index: usize, visible: usize) {
        let Some(matches) = &self.matches else { return };
        let Some(&(start, _)) = matches.get(index) else { return };
        self.current = Some(index);
        let line = file.line_of(start);
        if line < self.top || line >= self.top + visible.saturating_sub(1) {
            self.top = line.saturating_sub(visible / 3);
        }
        self.selection = Some((line, line));
        // Scroll sideways so the match is on screen.
        let line_start = file.line_start(line);
        let col = column_of(file.line(line), (start - line_start) as usize);
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
                let from = file.line_start(self.top.min(file.line_count().saturating_sub(1)));
                let i = matches.partition_point(|m| m.0 < from);
                if forward { i % n } else { (i + n - 1) % n }
            }
        };
        self.jump_to_match(file, next, visible);
    }

    fn copy_selection(&mut self, ctx: &egui::Context, file: &TextFile) {
        let Some((a, b)) = self.selection else { return };
        let (first, last) = (a.min(b), a.max(b));
        let (text, cut) = file.copy_lines(first, last, COPY_LIMIT);
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

    pub fn draw(&mut self, ui: &mut egui::Ui, file: &Arc<TextFile>) {
        let ctx = ui.ctx().clone();
        let total = file.line_count();
        let rows_guess = ((ui.available_height() - 40.0) / ROW_HEIGHT).max(1.0) as usize;
        self.poll(file, rows_guess);

        // Shortcuts that work anywhere in the window.
        let (ctrl_f, ctrl_g, f3, shift) = ui.input(|i| {
            (
                i.modifiers.command && i.key_pressed(Key::F),
                i.modifiers.command && i.key_pressed(Key::G),
                i.key_pressed(Key::F3),
                i.modifiers.shift,
            )
        });
        if ctrl_f {
            self.focus_search = true;
        }
        if ctrl_g {
            self.focus_goto = true;
        }
        if f3 {
            self.step(file, !shift, rows_guess);
        }

        // Toolbar.
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Find in file (Ctrl+F). Case-insensitive unless you type capitals.")
                    .desired_width((ui.available_width() - 470.0).max(160.0)),
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
        });
        if let Some(job) = &self.search_job {
            let done = job.progress.bytes.load(Ordering::Relaxed) as f32 / file.len().max(1) as f32;
            ui.add(egui::ProgressBar::new(done).text(format!("Searching… {:.0}%", done * 100.0)));
        }
        ui.add_space(4.0);

        if total == 0 {
            ui.centered_and_justified(|ui| ui.label(RichText::new("This file is empty.").weak().size(16.0)));
            return;
        }

        // Geometry.
        let area = ui.available_rect_before_wrap();
        let scrollbar_w = 12.0;
        let list = Rect::from_min_max(area.min, egui::pos2(area.max.x - scrollbar_w - 2.0, area.max.y));
        let bar = Rect::from_min_max(egui::pos2(area.max.x - scrollbar_w, area.min.y), area.max);
        let visible = ((list.height() / ROW_HEIGHT).floor() as usize).max(1);
        let max_top = total.saturating_sub(visible);
        let mono = FontId::monospace(13.0);
        let char_w = ui.ctx().fonts_mut(|f| f.glyph_width(&mono, '0')).max(1.0);
        let digits = group(total as u64).len();
        let gutter = digits as f32 * char_w + 16.0;
        let text_x = list.min.x + gutter + 6.0;
        let columns = (((list.max.x - text_x) / char_w).floor() as usize).max(1);
        let max_left = (file.longest_line as usize).saturating_sub(columns / 2);

        // Mouse wheel (vertical and sideways; Shift+wheel scrolls sideways).
        let response = ui.interact(list, ui.scope_id().with("text-lines"), Sense::click_and_drag());
        if ui.rect_contains_pointer(list) {
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
            if cols != 0.0 {
                self.scroll_remainder.x -= cols * char_w;
                self.left = (self.left as i64 + cols as i64).clamp(0, max_left as i64) as usize;
            }
        }

        // Keyboard, when no text field has focus.
        let typing = ui.ctx().memory(|m| m.focused().is_some());
        if !typing {
            let (down, up, pgdn, pgup, home, end, left, right, copy, select_all) = ui.input(|i| {
                (
                    i.key_pressed(Key::ArrowDown),
                    i.key_pressed(Key::ArrowUp),
                    i.key_pressed(Key::PageDown),
                    i.key_pressed(Key::PageUp),
                    i.key_pressed(Key::Home),
                    i.key_pressed(Key::End),
                    i.key_pressed(Key::ArrowLeft),
                    i.key_pressed(Key::ArrowRight),
                    i.events.iter().any(|e| matches!(e, egui::Event::Copy)),
                    i.modifiers.command && i.key_pressed(Key::A),
                )
            });
            let page = visible.saturating_sub(1).max(1);
            let mut moved = None;
            if let Some((_, cursor)) = self.selection {
                if down { moved = Some((cursor + 1).min(total - 1)); }
                if up { moved = Some(cursor.saturating_sub(1)); }
                if pgdn { moved = Some((cursor + page).min(total - 1)); }
                if pgup { moved = Some(cursor.saturating_sub(page)); }
            } else {
                if down { self.top = (self.top + 1).min(max_top); }
                if up { self.top = self.top.saturating_sub(1); }
                if pgdn { self.top = (self.top + page).min(max_top); }
                if pgup { self.top = self.top.saturating_sub(page); }
            }
            if home { moved = Some(0); self.left = 0; }
            if end { moved = Some(total - 1); }
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
            if left { self.left = self.left.saturating_sub(4); }
            if right { self.left = (self.left + 4).min(max_left); }
            if select_all {
                self.selection = Some((0, total - 1));
            }
            if copy {
                self.copy_selection(&ctx, file);
            }
        }
        self.top = self.top.min(max_top);

        // Clicks and drags select lines.
        let top_now = self.top;
        let pointer_line = move |p: egui::Pos2| ((p.y - list.min.y) / ROW_HEIGHT).floor().max(0.0) as usize + top_now;
        if let Some(p) = response.interact_pointer_pos() {
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
                // Drag past the edge to scroll.
                if p.y > list.max.y { self.top = (self.top + 1).min(max_top); }
                if p.y < list.min.y { self.top = self.top.saturating_sub(1); }
            }
        }
        if response.drag_stopped() {
            self.dragging = false;
        }
        if response.secondary_clicked() {
            if let Some(p) = response.interact_pointer_pos() {
                let line = pointer_line(p).min(total - 1);
                let inside = self.selection.is_some_and(|(a, b)| line >= a.min(b) && line <= a.max(b));
                if !inside {
                    self.selection = Some((line, line));
                }
            }
        }
        response.context_menu(|ui| {
            let Some((a, b)) = self.selection else {
                ui.close();
                return;
            };
            let (first, last) = (a.min(b), a.max(b));
            let label = if first == last { "Copy line".to_string() } else { format!("Copy {} lines", group((last - first + 1) as u64)) };
            if ui.button(format!("{label}    Ctrl+C")).clicked() {
                self.copy_selection(&ctx, file);
                ui.close();
            }
            if ui.button("Copy line number").clicked() {
                ctx.copy_text((first + 1).to_string());
                ui.close();
            }
            if ui.button("Select all    Ctrl+A").clicked() {
                self.selection = Some((0, total - 1));
                ui.close();
            }
        });

        // Draw.
        let painter = ui.painter_at(list);
        let visuals = ui.visuals().clone();
        let weak = visuals.weak_text_color();
        let hit_fill = if visuals.dark_mode { Color32::from_rgba_unmultiplied(230, 180, 40, 70) } else { Color32::from_rgba_unmultiplied(250, 200, 0, 110) };
        let current_fill = if visuals.dark_mode { Color32::from_rgba_unmultiplied(255, 140, 0, 150) } else { Color32::from_rgba_unmultiplied(255, 140, 0, 160) };
        painter.rect_filled(Rect::from_min_max(list.min, egui::pos2(list.min.x + gutter, list.max.y)), 0.0, visuals.faint_bg_color);
        let sel = self.selection.map(|(a, b)| (a.min(b), a.max(b)));
        let current_match = self.current.and_then(|i| self.matches.as_ref().and_then(|m| m.get(i).copied()));
        for row in 0..visible.min(total - self.top) {
            let line = self.top + row;
            let y = list.min.y + row as f32 * ROW_HEIGHT;
            let row_rect = Rect::from_min_size(egui::pos2(list.min.x, y), Vec2::new(list.width(), ROW_HEIGHT));
            if sel.is_some_and(|(a, b)| line >= a && line <= b) {
                painter.rect_filled(row_rect, 0.0, visuals.selection.bg_fill.gamma_multiply(0.45));
            }
            let mid = y + ROW_HEIGHT / 2.0;
            painter.text(egui::pos2(list.min.x + gutter - 8.0, mid), Align2::RIGHT_CENTER, (line + 1).to_string(), mono.clone(), weak);
            let bytes = file.line(line);

            // Highlight matches on this line.
            if let Some(matches) = &self.matches {
                let (ls, le) = file.line_range(line);
                let (ls, le) = (ls as u64, le as u64);
                let mut i = matches.partition_point(|m| m.1 <= ls);
                // Columns are counted incrementally from the previous match,
                // so a long line with many matches costs one pass at most.
                let (mut at, mut col) = (0usize, 0usize);
                while let Some(&(ms, me)) = matches.get(i) {
                    if ms >= le.max(ls + 1) || (ms - ls) as usize > HIGHLIGHT_REACH {
                        break;
                    }
                    let (s_off, e_off) = ((ms.max(ls) - ls) as usize, (me.min(le) - ls) as usize);
                    col = advance_column(bytes, at, s_off.max(at), col);
                    at = s_off.max(at);
                    let c0 = col;
                    let c1 = advance_column(bytes, at, e_off.max(at), col).max(c0 + 1);
                    if c0 >= self.left + columns {
                        break;
                    }
                    if c1 > self.left {
                        let x0 = text_x + c0.saturating_sub(self.left) as f32 * char_w;
                        let x1 = text_x + (c1 - self.left).min(columns) as f32 * char_w;
                        let fill = if current_match == Some((ms, me)) { current_fill } else { hit_fill };
                        painter.rect_filled(Rect::from_min_max(egui::pos2(x0, y + 1.0), egui::pos2(x1, y + ROW_HEIGHT - 1.0)), 2.0, fill);
                    }
                    i += 1;
                }
            }
            let shown = text::display_window(bytes, self.left, columns + 1);
            painter.text(egui::pos2(text_x, mid), Align2::LEFT_CENTER, shown, mono.clone(), visuals.text_color());
        }

        if let Some(top) = scrollbar(ui, bar, self.top, total, visible, ui.scope_id().with("text-scrollbar")) {
            self.top = top;
        }
        painter.rect_stroke(list, 4.0, Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color), egui::StrokeKind::Outside);
        ui.allocate_rect(area, Sense::hover());
    }
}

/// Display column (tabs expanded, 4 wide) of byte `offset` in `line`.
fn column_of(line: &[u8], offset: usize) -> usize {
    advance_column(line, 0, offset, 0)
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
