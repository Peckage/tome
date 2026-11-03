use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::engine::{self, Doc, Error, Format, Progress};

const USAGE: &str = "\
Tome: open, search and convert huge text files.

Usage:
    tome                               open the window
    tome FILE.xml                      open the window with a file
    tome INPUT OUTPUT [options]        convert on the command line

Options:
    --tag NAME     element to emit as one record (default: part)
    --pretty       indented JSON array (like json.dumps(..., indent=2))
    --ndjson       one JSON object per line
    --quiet        no progress output
    -h, --help     show this help

Without --pretty or --ndjson the output is a JSON array with one compact
record per line.";

pub struct Args {
    pub input: PathBuf,
    pub output: PathBuf,
    pub tag: String,
    pub format: Format,
    pub quiet: bool,
}

pub enum Command {
    Gui(Option<PathBuf>),
    Convert(Args),
    Exit(i32),
}

pub fn parse(argv: Vec<String>) -> Command {
    let mut positional = Vec::new();
    let mut tag = String::from("part");
    let mut pretty = false;
    let mut ndjson = false;
    let mut quiet = false;
    let mut it = argv.into_iter().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Command::Exit(0);
            }
            "--pretty" => pretty = true,
            "--ndjson" => ndjson = true,
            "--quiet" | "-q" => quiet = true,
            "--tag" => match it.next() {
                Some(value) => tag = value,
                None => return usage_error("--tag needs a value"),
            },
            a if a.starts_with("--tag=") => tag = a[6..].to_string(),
            "--" => positional.extend(it.by_ref()),
            a if a.starts_with('-') && a.len() > 1 => return usage_error(&format!("unknown option {a}")),
            _ => positional.push(arg),
        }
    }
    if pretty && ndjson {
        return usage_error("--pretty and --ndjson cannot be combined");
    }
    if tag.is_empty() {
        return usage_error("--tag cannot be empty");
    }
    let format = if pretty {
        Format::Pretty
    } else if ndjson {
        Format::Ndjson
    } else {
        Format::Compact
    };
    match positional.len() {
        0 => Command::Gui(None),
        1 => Command::Gui(Some(PathBuf::from(&positional[0]))),
        2 => Command::Convert(Args {
            input: PathBuf::from(&positional[0]),
            output: PathBuf::from(&positional[1]),
            tag,
            format,
            quiet,
        }),
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
        let result = engine::convert(&doc, &args.tag, &args.output, args.format, &progress);
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
