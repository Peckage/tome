mod cli;
mod engine;
mod gui;
mod json;
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
    let code = match cli::parse(argv) {
        cli::Command::Convert(args) => cli::run(args),
        cli::Command::Gui(path) => gui::run(path),
        cli::Command::Exit(code) => code,
    };
    std::process::exit(code);
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
