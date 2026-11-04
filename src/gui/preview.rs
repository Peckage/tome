//! The live preview: the whole file shown in the selected output format,
//! exactly as saving would write it, rendered only where you look.
//!
//! The output is a list of blocks (header, one per record, footer) that,
//! joined with newlines, are the output file. Only blocks on screen are
//! rendered (and cached), so switching format or scrolling through millions
//! of records is instant.

use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui::{self, Align2, FontId, Key, Rect, RichText, Sense, Stroke, Vec2};

use super::{ROW_HEIGHT, scrollbar};
use crate::cli::group;
use crate::engine::{Doc, Encoder};
use crate::text;

/// Rendered blocks kept around; older ones are dropped.
const CACHE_LIMIT: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Block {
    Header,
    Record(usize),
    Footer,
}

#[derive(Default)]
pub struct PreviewView {
    /// Position: block index and line inside it.
    block: usize,
    line: usize,
    left: usize,
    scroll_remainder: Vec2,
    cache: HashMap<usize, Arc<Vec<String>>>,
    pub selected_block: Option<usize>,
    pub status: Option<(String, bool)>,
}

/// What the window needs to draw a preview.
pub struct Source<'a> {
    doc: &'a Doc,
    ranges: &'a [(usize, usize)],
    encoder: &'a Encoder,
    header: Option<String>,
    footer: Option<String>,
}

impl<'a> Source<'a> {
    pub fn new(doc: &'a Doc, ranges: &'a [(usize, usize)], encoder: &'a Encoder) -> Self {
        Source { doc, ranges, encoder, header: encoder.preview_header(), footer: encoder.preview_footer() }
    }

    fn has_header(&self) -> bool {
        self.header.is_some()
    }

    fn block_count(&self) -> usize {
        self.has_header() as usize + self.ranges.len() + self.footer.is_some() as usize
    }

    fn kind(&self, block: usize) -> Block {
        let h = self.has_header() as usize;
        if block < h {
            Block::Header
        } else if block - h < self.ranges.len() {
            Block::Record(block - h)
        } else {
            Block::Footer
        }
    }

    fn block_of_record(&self, record: usize) -> usize {
        record + self.has_header() as usize
    }

    fn render(&self, block: usize) -> Vec<String> {
        let text = match self.kind(block) {
            Block::Header => self.header.clone().unwrap_or_default(),
            Block::Footer => self.footer.clone().unwrap_or_default(),
            Block::Record(i) => match self.encoder.preview_block(self.doc, self.ranges[i], i, self.ranges.len()) {
                Ok(t) => t,
                Err(e) => format!("error in record #{}: {e}", group(i as u64 + 1)),
            },
        };
        text.split('\n').map(String::from).collect()
    }
}

impl PreviewView {
    /// The format, options or records changed: drop everything rendered.
    pub fn invalidate(&mut self) {
        self.cache.clear();
        self.line = 0;
    }

    pub fn reset(&mut self) {
        *self = PreviewView::default();
    }

    fn lines(&mut self, src: &Source, block: usize) -> Arc<Vec<String>> {
        if let Some(l) = self.cache.get(&block) {
            return l.clone();
        }
        if self.cache.len() >= CACHE_LIMIT {
            self.cache.clear();
        }
        let lines = Arc::new(src.render(block));
        self.cache.insert(block, lines.clone());
        lines
    }

    /// Move the view by `delta` lines.
    fn scroll_lines(&mut self, src: &Source, delta: i64) {
        let blocks = src.block_count();
        if blocks == 0 {
            return;
        }
        let mut line = self.line as i64 + delta;
        while line < 0 && self.block > 0 {
            self.block -= 1;
            line += self.lines(src, self.block).len() as i64;
        }
        let mut line = line.max(0) as usize;
        loop {
            let len = self.lines(src, self.block).len();
            if line < len || self.block + 1 >= blocks {
                self.line = line.min(len.saturating_sub(1));
                break;
            }
            line -= len;
            self.block += 1;
        }
    }

    /// Show `record` near the top.
    pub fn reveal(&mut self, src: &Source, record: usize) {
        self.block = src.block_of_record(record).min(src.block_count().saturating_sub(1));
        self.line = 0;
        self.selected_block = Some(self.block);
    }

    /// Draw; returns a record the user clicked.
    pub fn draw(&mut self, ui: &mut egui::Ui, src: &Source) -> Option<u32> {
        let ctx = ui.ctx().clone();
        let blocks = src.block_count();
        self.block = self.block.min(blocks.saturating_sub(1));
        let mut clicked_record = None;

        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Preview as {}", src.encoder.format.name())).strong());
            ui.label(RichText::new("· exactly what Save writes · change the format above to view it differently").weak());
        });
        ui.add_space(4.0);

        let area = ui.available_rect_before_wrap();
        let scrollbar_w = 12.0;
        let list = Rect::from_min_max(area.min, egui::pos2(area.max.x - scrollbar_w - 2.0, area.max.y));
        let bar = Rect::from_min_max(egui::pos2(area.max.x - scrollbar_w, area.min.y), area.max);
        let visible = ((list.height() / ROW_HEIGHT).floor() as usize).max(1);
        let mono = FontId::monospace(13.0);
        let char_w = ui.ctx().fonts_mut(|f| f.glyph_width(&mono, '0')).max(1.0);
        let gutter = (group(src.ranges.len() as u64).len() + 1) as f32 * char_w + 16.0;
        let text_x = list.min.x + gutter + 6.0;
        let columns = (((list.max.x - text_x) / char_w).floor() as usize).max(1);

        // Input.
        let response = ui.interact(list, ui.scope_id().with("preview-lines"), Sense::click());
        if ui.rect_contains_pointer(list) {
            let (mut delta, shift) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers.shift));
            if shift && delta.x == 0.0 {
                delta = Vec2::new(delta.y, 0.0);
            }
            self.scroll_remainder -= delta;
            let rows = (self.scroll_remainder.y / ROW_HEIGHT).trunc();
            if rows != 0.0 {
                self.scroll_remainder.y -= rows * ROW_HEIGHT;
                self.scroll_lines(src, rows as i64);
            }
            let cols = (self.scroll_remainder.x / char_w).trunc();
            if cols != 0.0 {
                self.scroll_remainder.x -= cols * char_w;
                self.left = (self.left as i64 + cols as i64).max(0) as usize;
            }
        }
        let typing = ui.ctx().memory(|m| m.focused().is_some());
        if !typing {
            let (down, up, pgdn, pgup, home, end, left, right, copy) = ui.input(|i| {
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
                )
            });
            let page = visible.saturating_sub(1).max(1) as i64;
            if down { self.scroll_lines(src, 1); }
            if up { self.scroll_lines(src, -1); }
            if pgdn { self.scroll_lines(src, page); }
            if pgup { self.scroll_lines(src, -page); }
            if home {
                self.block = 0;
                self.line = 0;
                self.left = 0;
            }
            if end {
                self.block = blocks.saturating_sub(1);
                self.line = 0;
            }
            if left { self.left = self.left.saturating_sub(4); }
            if right { self.left += 4; }
            if copy {
                if let Some(b) = self.selected_block {
                    let text = self.lines(src, b).join("\n");
                    ctx.copy_text(text);
                    self.status = Some(match src.kind(b) {
                        Block::Record(i) => (format!("Copied record #{} as {}.", group(i as u64 + 1), src.encoder.format.name()), false),
                        _ => ("Copied.".into(), false),
                    });
                }
            }
        }

        // Draw the rows, walking blocks from the current position.
        let painter = ui.painter_at(list);
        let visuals = ui.visuals().clone();
        let weak = visuals.weak_text_color();
        painter.rect_filled(Rect::from_min_max(list.min, egui::pos2(list.min.x + gutter, list.max.y)), 0.0, visuals.faint_bg_color);
        let pointer = ui.input(|i| i.pointer.interact_pos());
        let (mut block, mut line) = (self.block, self.line);
        let mut row = 0;
        while row < visible && block < blocks {
            let lines = self.lines(src, block);
            let selected = self.selected_block == Some(block);
            while line < lines.len() && row < visible {
                let y = list.min.y + row as f32 * ROW_HEIGHT;
                let row_rect = Rect::from_min_size(egui::pos2(list.min.x, y), Vec2::new(list.width(), ROW_HEIGHT));
                if selected {
                    painter.rect_filled(row_rect, 0.0, visuals.selection.bg_fill.gamma_multiply(0.35));
                }
                if response.clicked() && pointer.is_some_and(|p| row_rect.contains(p)) {
                    self.selected_block = Some(block);
                    if let Block::Record(i) = src.kind(block) {
                        clicked_record = Some(i as u32);
                    }
                }
                let mid = y + ROW_HEIGHT / 2.0;
                if line == 0 {
                    if let Block::Record(i) = src.kind(block) {
                        painter.text(egui::pos2(list.min.x + gutter - 8.0, mid), Align2::RIGHT_CENTER, format!("#{}", group(i as u64 + 1)), mono.clone(), weak);
                    }
                }
                let shown = text::display_window(lines[line].as_bytes(), self.left, columns + 1);
                painter.text(egui::pos2(text_x, mid), Align2::LEFT_CENTER, shown, mono.clone(), visuals.text_color());
                row += 1;
                line += 1;
            }
            block += 1;
            line = 0;
        }

        if let Some(top) = scrollbar(ui, bar, self.block, blocks, 1, ui.scope_id().with("preview-scrollbar")) {
            self.block = top;
            self.line = 0;
        }
        painter.rect_stroke(list, 4.0, Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color), egui::StrokeKind::Outside);
        ui.allocate_rect(area, Sense::hover());
        clicked_record
    }
}
