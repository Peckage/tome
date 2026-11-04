use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::engine::{self, Doc, Error, Format, Kind, Progress};
use crate::node::Options;

const USAGE: &str = "\
Tome: open, search and convert huge text files.

Usage:
    tome                               open the window
    tome FILE                          open the window with any file
    tome INPUT OUTPUT [options]        convert XML or JSON on the command line

Input is XML or JSON (a document, NDJSON or concatenated values), detected
from the content. The output format follows the output file's extension
(.ndjson .jsonl .yaml .yml .xml .csv .tsv); .json gives compact JSON.

Records:
    --tag NAME          XML element to emit as one record (default: part)
    --records PATH      JSON key path holding the records, e.g. data.items;
                        \"$\" is the whole document (default: the root array,
                        or the biggest array under the root object)

Format:
    --format NAME       json, pretty, ndjson, yaml, xml, csv or tsv
    --pretty            same as --format pretty
    --ndjson            same as --format ndjson

Clean-up (all off by default):
    --trim              trim spaces from the start and end of every value
    --collapse-spaces   turn every run of whitespace into one space
    --remove-empty      drop empty values, nulls, {} and [] everywhere
    --unwrap            replace one-item lists by their item
    --text-values       replace {\"#text\": v} by v
    --attr-prefix P     put P in front of XML attribute names, e.g. @
    --drop KEYS         remove these keys everywhere (comma-separated)
    --simple            --unwrap --text-values --attr-prefix @ (xmltodict style)

Other:
    --quiet             no progress output
    -h, --help          show this help";

static VIA_PREVIEW: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub struct Args {
    pub input: PathBuf,
    pub output: PathBuf,
    pub selector: Option<String>,
    pub format: Format,
    pub options: Options,
    pub quiet: bool,
}

pub enum Command {
    Gui(Option<PathBuf>),
    Convert(Args),
    Exit(i32),
}

fn parse_format(name: &str) -> Option<Format> {
    match name.to_ascii_lowercase().as_str() {
        "json" | "compact" => Some(Format::Compact),
        "pretty" => Some(Format::Pretty),
        other => Format::from_extension(other),
    }
}

pub fn parse(argv: Vec<String>) -> Command {
    let mut positional = Vec::new();
    let mut selector: Option<String> = None;
    let mut format: Option<Format> = None;
    let mut options = Options::default();
    let mut quiet = false;
    let mut it = argv.into_iter().skip(1);
    while let Some(arg) = it.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |what: &str| -> Result<String, Command> {
            match inline.clone().or_else(|| it.next()) {
                Some(v) => Ok(v),
                None => Err(usage_error(&format!("{what} needs a value"))),
            }
        };
        let mut set_format = |f: Format| -> Result<(), Command> {
            match format {
                Some(old) if old != f => Err(usage_error("choose one output format")),
                _ => {
                    format = Some(f);
                    Ok(())
                }
            }
        };
        let step = match name.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Command::Exit(0);
            }
            "--pretty" => set_format(Format::Pretty),
            "--ndjson" => set_format(Format::Ndjson),
            "--format" => value("--format").and_then(|v| match parse_format(&v) {
                Some(f) => set_format(f),
                None => Err(usage_error(&format!("unknown format {v}"))),
            }),
            "--quiet" | "-q" => {
                quiet = true;
                Ok(())
            }
            "--tag" | "--records" => value(&name).map(|v| selector = Some(v)),
            "--trim" => Ok(options.trim = true),
            "--collapse-spaces" => Ok(options.collapse_spaces = true),
            "--remove-empty" => Ok(options.remove_empty = true),
            "--unwrap" => Ok(options.unwrap_single = true),
            "--text-values" => Ok(options.text_values = true),
            "--attr-prefix" => value("--attr-prefix").map(|v| options.attr_prefix = v),
            "--drop" => value("--drop").map(|v| {
                let mut more = Options::default();
                more.set_drop_keys(&v);
                options.drop_keys.extend(more.drop_keys);
            }),
            "--simple" => {
                let s = Options::simple();
                options.unwrap_single = true;
                options.text_values = true;
                options.attr_prefix = s.attr_prefix;
                Ok(())
            }
            "--via-preview" => {
                // Hidden test hook: write through the live-preview renderer.
                VIA_PREVIEW.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            "--generic" => {
                // Hidden test hook: force the generic writer path.
                crate::engine::FORCE_GENERIC.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            "--" => {
                positional.extend(it.by_ref());
                Ok(())
            }
            a if a.starts_with('-') && a.len() > 1 => Err(usage_error(&format!("unknown option {a}"))),
            _ => {
                positional.push(arg);
                Ok(())
            }
        };
        if let Err(command) = step {
            return command;
        }
    }
    match positional.len() {
        0 => Command::Gui(None),
        1 => Command::Gui(Some(PathBuf::from(&positional[0]))),
        2 => {
            let output = PathBuf::from(&positional[1]);
            let format = format
                .or_else(|| output.extension().and_then(|e| Format::from_extension(&e.to_string_lossy())))
                .unwrap_or(Format::Compact);
            Command::Convert(Args { input: PathBuf::from(&positional[0]), output, selector, format, options, quiet })
        }
        _ => usage_error("too many arguments"),
    }
}

fn usage_error(message: &str) -> Command {
    eprintln!("error: {message}\n\n{USAGE}");
    Command::Exit(2)
}

pub fn run(args: Args) -> i32 {
    let started = Instant::now();
    let doc = match Doc::open(&args.input) {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!("error: {}: {e}", args.input.display());
            return 1;
        }
    };
    let selector = match (&args.selector, doc.kind) {
        (Some(s), _) => s.trim().to_string(),
        (None, kind) => kind.default_selector().to_string(),
    };
    if doc.kind == Kind::Xml && selector.is_empty() {
        eprintln!("error: --tag cannot be empty");
        return 2;
    }
    let progress = Progress::default();
    let show = !args.quiet && std::io::stderr().is_terminal();
    let total = doc.bytes().len().max(1) as f64;

    let done = std::sync::atomic::AtomicBool::new(false);
    let result = std::thread::scope(|scope| {
        let reporter = show.then(|| {
            let (progress, done) = (&progress, &done);
            scope.spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(200));
                    let fraction = progress.bytes.load(Ordering::Relaxed) as f64 / total;
                    let records = progress.records.load(Ordering::Relaxed);
                    eprint!("\r{:>5.1}%  {} records", fraction * 100.0, group(records));
                }
                eprint!("\r\x1b[K");
            })
        });
        let result = if VIA_PREVIEW.load(std::sync::atomic::Ordering::Relaxed) {
            engine::write_via_preview(&doc, &selector, &args.output, args.format, &args.options)
        } else {
            engine::convert(&doc, &selector, &args.output, args.format, &args.options, &progress)
        };
        done.store(true, Ordering::Relaxed);
        if let Some(handle) = reporter {
            let _ = handle.join();
        }
        result
    });

    match result {
        Ok(count) => {
            if !args.quiet {
                eprintln!(
                    "wrote {} records to {} in {:.2} s",
                    group(count),
                    args.output.display(),
                    started.elapsed().as_secs_f64()
                );
            }
            0
        }
        Err(Error::Cancelled) => {
            eprintln!("cancelled");
            130
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// 1234567 -> "1,234,567"
pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}
