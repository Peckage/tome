//! Screenshot mode for the README (build with `--features screenshots`).
//!
//! `TOME_SCREENSHOT=out.png TOME_DEMO=<scene> tome FILE` opens the file,
//! sets up the scene once everything has loaded, waits for every background
//! job to finish, saves a screenshot of the window and exits.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui;

use super::{App, View};
use crate::engine::Format;

pub struct Demo {
    out: PathBuf,
    scene: String,
    frames: u32,
    stage: u8,
    saved: Arc<AtomicBool>,
}

impl Demo {
    pub fn from_env() -> Option<Demo> {
        let out = std::env::var_os("TOME_SCREENSHOT")?;
        Some(Demo { out: out.into(), scene: std::env::var("TOME_DEMO").unwrap_or_default(), frames: 0, stage: 0, saved: Arc::default() })
    }
}

fn idle(app: &App) -> bool {
    app.open_job.is_none()
        && app.text_job.is_none()
        && app.index_job.is_none()
        && app.search_job.is_none()
        && app.convert_job.is_none()
        && app.columns_job.is_none()
        && app.save_job.is_none()
        && !app.textview.busy()
        && app.text.is_some()
}

pub fn step(app: &mut App, ctx: &egui::Context) {
    let Some(demo) = app.demo.as_mut() else { return };
    ctx.request_repaint();
    demo.frames += 1;
    let (scene, stage, frames) = (demo.scene.clone(), demo.stage, demo.frames);
    match stage {
        0 if frames > 5 && idle(app) && (app.kind.is_none() || app.ranges.is_some()) => {
            setup(app, ctx, &scene);
            let demo = app.demo.as_mut().unwrap();
            demo.stage = 1;
            demo.frames = 0;
        }
        1 if frames > 20 && idle(app) && (scene != "records" && scene != "preview" || app.encoder.is_some()) => {
            let demo = app.demo.as_mut().unwrap();
            demo.stage = 2;
            let (out, saved, ctx2) = (demo.out.clone(), demo.saved.clone(), ctx.clone());
            ctx.request_screenshot(move |image| {
                if let Err(e) = save_png(&out, &image) {
                    eprintln!("screenshot failed: {e}");
                }
                saved.store(true, Ordering::Relaxed);
                ctx2.request_repaint();
            });
        }
        2 if demo_saved(app) => {
            app.allow_close = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            app.demo.as_mut().unwrap().stage = 3;
        }
        _ => {}
    }
}

fn demo_saved(app: &App) -> bool {
    app.demo.as_ref().is_some_and(|d| d.saved.load(Ordering::Relaxed))
}

fn setup(app: &mut App, ctx: &egui::Context, scene: &str) {
    let text = app.text.clone();
    // Show a plain output name, not the folder the demo files live in.
    if let Some(name) = app.path.as_ref().and_then(|p| p.file_stem()) {
        let format = if scene == "csv" { Format::Csv } else if scene == "preview" { Format::Yaml } else { Format::Pretty };
        app.output = format!("{}.{}", name.to_string_lossy(), format.extension());
    }
    match scene {
        "text" => {
            app.view = View::Text;
            app.view_chosen = true;
            if let Some(text) = text {
                // Show an edit (margin mark, "unsaved") and search highlights.
                app.textview.edits.set_line(3, "2026-11-04T10:03:03.003 WARN worker-3 edited in Tome: this line is new".into());
                app.textview.search = "ERROR".into();
                app.textview.start_search(ctx, &text);
            }
        }
        "records" => {
            app.view = View::Records;
            app.view_chosen = true;
            app.format = Format::Pretty;
            app.toggle_record(1);
            app.select(1);
        }
        "preview" => {
            app.format = Format::Yaml;
            app.view = View::Preview;
            app.view_chosen = true;
            app.select(2);
        }
        "csv" => {
            app.format = Format::Csv;
            app.options.unwrap_single = true;
            app.options.text_values = true;
            app.view = View::Preview;
            app.view_chosen = true;
            app.select(0);
        }
        _ => {}
    }
}

fn save_png(path: &std::path::Path, image: &egui::ColorImage) -> std::io::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, image.size[0] as u32, image.size[1] as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(std::io::Error::other)?;
    writer.write_image_data(image.as_raw()).map_err(std::io::Error::other)
}
