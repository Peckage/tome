// Windows: tome.exe is a GUI program, so starting it from Explorer, the
// Start menu or "Open with" opens no console window. tome-cli.exe is the same
// program built as a console program (the "console" feature) for terminals,
// where the shell waits for it, shows its output and gets its exit code.
#![cfg_attr(all(windows, not(test), not(feature = "console")), windows_subsystem = "windows")]

mod cli;
mod edit;
mod engine;
mod gui;
mod json;
mod jsonin;
mod node;
mod pscan;
mod text;
mod xml;

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("--bench-scan") {
        // Hidden: time the stages separately on argv[2] with tag argv[3].
        let t = std::time::Instant::now();
        let doc = engine::Doc::open(std::path::Path::new(&argv[2])).unwrap();
        eprintln!("open+validate {:.3}s", t.elapsed().as_secs_f64());
        let t = std::time::Instant::now();
        let lines = engine::count_lines(std::path::Path::new(&argv[2])).unwrap();
        eprintln!("count lines   {:.3}s ({lines})", t.elapsed().as_secs_f64());
        let t = std::time::Instant::now();
        let index = engine::index_records(&doc, &argv[3]).unwrap();
        eprintln!("parallel scan {:.3}s ({} records)", t.elapsed().as_secs_f64(), index.len());
        let t = std::time::Instant::now();
        let mut scanner = xml::Scanner::new(doc.bytes(), doc.origin, &argv[3], &doc.entities);
        let mut n = 0;
        while let Some(_) = scanner.next_record().unwrap() { n += 1; }
        eprintln!("serial scan   {:.3}s ({n} records)", t.elapsed().as_secs_f64());
        std::process::exit(0);
    }
    if argv.get(1).map(String::as_str) == Some("--bench-text") {
        // Hidden: time opening argv[2] as text and searching it for argv[3].
        let t = std::time::Instant::now();
        let file = text::TextFile::open(std::path::Path::new(&argv[2])).unwrap();
        eprintln!("open+index    {:.3}s ({} lines, longest {} bytes)", t.elapsed().as_secs_f64(), file.line_count(), file.longest_line);
        for regex in [false, true] {
            let t = std::time::Instant::now();
            let p = text::Pattern::new(&argv[3], regex).unwrap();
            let (hits, _) = text::find_all(&file, &p, 10_000_000, &engine::Progress::default()).unwrap();
            eprintln!("search{}  {:.3}s ({} hits)", if regex { " (re)" } else { "     " }, t.elapsed().as_secs_f64(), hits.len());
        }
        std::process::exit(0);
    }
    if argv.get(1).map(String::as_str) == Some("--check-scan") {
        // Hidden: compare the parallel scanner with the sequential one on
        // argv[2] (tag argv[3]) at several chunk sizes. Used by the tests.
        std::process::exit(check_scan(&argv[2], &argv[3]));
    }
    // tome.exe on Windows has no console of its own: borrow the terminal's
    // for command-line use, so --help and errors are visible there.
    #[cfg(all(windows, not(feature = "console")))]
    if argv.len() > 2 || argv.get(1).is_some_and(|a| a.starts_with('-')) {
        attach_console();
    }
    let code = match cli::parse(argv) {
        cli::Command::Convert(args) => cli::run(args),
        cli::Command::Gui(path) => gui::run(path),
        cli::Command::Exit(code) => code,
    };
    std::process::exit(code);
}

/// Attach to the console of the program that started us, if any, and point
/// stdout/stderr at it unless they were redirected.
#[cfg(all(windows, not(feature = "console")))]
fn attach_console() {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle};
    // SAFETY: plain Win32 calls with valid arguments; the handle is kept
    // open for the rest of the process.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        let name: Vec<u16> = "CONOUT$\0".encode_utf16().collect();
        let mut console = INVALID_HANDLE_VALUE;
        for id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let current = GetStdHandle(id);
            if current.is_null() || current == INVALID_HANDLE_VALUE {
                if console == INVALID_HANDLE_VALUE {
                    console = CreateFileW(
                        name.as_ptr(),
                        GENERIC_READ | GENERIC_WRITE,
                        FILE_SHARE_READ | FILE_SHARE_WRITE,
                        std::ptr::null(),
                        OPEN_EXISTING,
                        0,
                        std::ptr::null_mut(),
                    );
                }
                if console != INVALID_HANDLE_VALUE {
                    SetStdHandle(id, console);
                }
            }
        }
        // The shell has already printed its next prompt; start below it.
        // (Only on the console: redirected output stays exactly as is.)
        if console != INVALID_HANDLE_VALUE && GetStdHandle(STD_OUTPUT_HANDLE) == console {
            println!();
        }
    }
}

fn check_scan(path: &str, tag: &str) -> i32 {
    let doc = match engine::Doc::open(std::path::Path::new(path)) {
        Ok(d) => d,
        Err(_) => return 0, // rejected before scanning: nothing to compare
    };
    let mut scanner = xml::Scanner::new(doc.bytes(), doc.origin, tag, &doc.entities);
    let mut expected = Vec::new();
    let serial_error = loop {
        match scanner.next_record() {
            Ok(Some(r)) => expected.push(r),
            Ok(None) => break None,
            Err(e) => break Some(e),
        }
    };
    for chunk in [1usize, 2, 3, 7, 16, 61, 256, 4096, 1 << 20] {
        let got = pscan::scan(doc.bytes(), doc.origin, tag, &doc.entities, chunk);
        let same = match (&serial_error, &got.error) {
            (None, None) => got.ranges == expected,
            (Some(a), Some(b)) => a.offset == b.offset && got.ranges.iter().all(|r| expected.contains(r)),
            _ => false,
        };
        if !same {
            println!(
                "MISMATCH chunk={chunk}: serial {} records, error {:?}; parallel {} records, error {:?}",
                expected.len(), serial_error, got.ranges.len(), got.error
            );
            return 1;
        }
    }
    0
}
