//! fpython — a strictly typed, natively compiled Python 4.x-style language.

mod ast;
mod bundle;
mod compiler;
mod diag;
mod doc;
mod driver;
mod convert;
mod fmt;
mod lint;
mod lexer;
mod parser;
mod project;
mod pycomp;
mod types;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "fpython — strictly typed, natively compiled Python

USAGE:
    fpython <command> [options] [args]

COMMANDS:
    run <file.fpy> [args...]     Compile and run a program
    build <file.fpy> [options]   Compile to a native executable
        -o <path>                  Output path
        --release                  Optimize for speed (-O3)
        --static                   Link statically (for FROM scratch images)
        --target <triple>          Cross-compile target (passed to the C compiler)
        --cc <compiler>            C compiler to use (default: $FPY_CC, $CC or cc)
        --emit-c                   Keep the generated C next to the output
    check <file.fpy>             Type-check without producing a binary
    fmt [--check] <paths...>     Format .fpy files in place
    lint <paths...>              Report style and correctness problems
    doc [paths...] [options]     Generate API docs from docstrings (default: HTML in build/docs)
        -o <dir>                   Output directory
        --format html|md           HTML site (with search) or Markdown files
        --private                  Include _private names
        --title <text>             Title of the index page
        --check                    List public items without docstrings (exit 1 if any)
        --stdout                   Print Markdown for the given files instead of writing
    convert <paths...> [-o dir]  Convert Python (.py) to fpython (.fpy)
    bundle <exe> <rootfs>        Collect exe + shared libs + Python home into a FROM scratch rootfs
    init <name>                  Create a new project
    test [dir]                   Run test_*.fpy files
    version                      Print the version
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");
    let rest = if args.len() > 2 { &args[2..] } else { &[][..] };
    match cmd {
        "run" => cmd_run(rest),
        "build" => cmd_build(rest),
        "check" => cmd_check(rest),
        "version" | "--version" | "-V" => {
            println!("fpython {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "fmt" => cmd_fmt(rest),
        "lint" => cmd_lint(rest),
        "doc" => cmd_doc(rest),
        "convert" => cmd_convert(rest),
        "init" => cmd_init(rest),
        "bundle" => cmd_bundle(rest),
        "test" => cmd_test(rest),
        "fmt-verify" => cmd_fmt_verify(rest),
        "ast-fingerprint" => {
            println!("{}", fingerprint_file(&rest[0]));
            ExitCode::SUCCESS
        }
        "pycomp" => {
            // debug: print the C generated for a pure-Python module
            let src = std::fs::read_to_string(&rest[1]).unwrap_or_default();
            match pycomp::compile_module(&rest[0], &rest[1], &src) {
                Ok(m) => {
                    println!("{}", m.c_code);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{}", e);
                    ExitCode::FAILURE
                }
            }
        }
        "parse" => {
            let mut fails = 0;
            for f in rest {
                if let Ok(src) = std::fs::read_to_string(f) {
                    if let Err(d) = parser::parse(&src, f) {
                        eprint!("{}", d.render(Some(&src)));
                        fails += 1;
                    }
                }
            }
            eprintln!("{} failures / {}", fails, rest.len());
            ExitCode::SUCCESS
        }
        "help" | "--help" | "-h" => {
            print!("{}", USAGE);
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("fpython: unknown command '{}'\n\n{}", other, USAGE);
            ExitCode::from(2)
        }
    }
}

fn parse_build_args(rest: &[String]) -> Result<(PathBuf, driver::BuildOpts, Vec<String>), String> {
    let mut opts = driver::BuildOpts::default();
    let mut entry: Option<PathBuf> = None;
    let mut prog_args = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        if entry.is_some() && !a.starts_with('-') {
            prog_args.extend(rest[i..].iter().cloned());
            break;
        }
        match a.as_str() {
            "-o" => {
                i += 1;
                opts.output = Some(PathBuf::from(rest.get(i).ok_or("-o needs a path")?));
            }
            "--release" => opts.opt_level = "-O3".into(),
            "--debug" => {
                opts.opt_level = "-O0".into();
                opts.debug = true;
            }
            "--static" => opts.static_link = true,
            "--emit-c" => opts.emit_c = true,
            "--target" => {
                i += 1;
                opts.target = Some(rest.get(i).ok_or("--target needs a value")?.clone());
            }
            "--cc" => {
                i += 1;
                opts.cc = rest.get(i).ok_or("--cc needs a value")?.clone();
            }
            "--" => {
                prog_args.extend(rest[i + 1..].iter().cloned());
                break;
            }
            s if s.starts_with("-") && entry.is_some() => {
                prog_args.extend(rest[i..].iter().cloned());
                break;
            }
            s if s.starts_with("-") => return Err(format!("unknown option '{}'", s)),
            s => entry = Some(PathBuf::from(s)),
        }
        i += 1;
    }
    let entry = match entry {
        Some(e) => e,
        None => {
            // no file given: use the project's fpython.toml
            let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
            let proj = project::find_project(&cwd).ok_or("missing input file (and no fpython.toml found)")??;
            if !rest.iter().any(|a| a == "--release" || a == "--debug") && proj.release {
                opts.opt_level = "-O3".into();
            }
            if proj.static_link {
                opts.static_link = true;
            }
            if opts.output.is_none() {
                opts.output = Some(proj.output.clone().unwrap_or_else(|| proj.root.join("build").join(&proj.name)));
            }
            proj.entry
        }
    };
    Ok((entry, opts, prog_args))
}

fn cmd_build(rest: &[String]) -> ExitCode {
    let (entry, opts, _) = match parse_build_args(rest) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("fpython build: {}", e);
            return ExitCode::from(2);
        }
    };
    match driver::build(&entry, &opts) {
        Ok(out) => {
            eprintln!("built {}", out.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprint!("{}", e);
            ExitCode::FAILURE
        }
    }
}

fn cmd_run(rest: &[String]) -> ExitCode {
    let (entry, mut opts, prog_args) = match parse_build_args(rest) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("fpython run: {}", e);
            return ExitCode::from(2);
        }
    };
    let dir = std::env::temp_dir().join(format!("fpython-run-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let stem = entry.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "prog".into());
    let exe = dir.join(&stem);
    opts.output = Some(exe.clone());
    let res = driver::build(&entry, &opts);
    let code = match res {
        Ok(_) => match Command::new(&exe)
            .args(&prog_args)
            .env("FPY_SOURCE_DIR", entry.canonicalize().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())).unwrap_or_else(|| PathBuf::from(".")))
            .status()
        {
            Ok(st) => match st.code() {
                Some(c) => ExitCode::from(c as u8),
                None => {
                    // killed by a signal: report it like a shell would (128 + signal number)
                    use std::os::unix::process::ExitStatusExt;
                    let sig = st.signal().unwrap_or(0);
                    eprintln!("fpython run: program terminated by signal {}", sig);
                    ExitCode::from((128 + sig) as u8)
                }
            },
            Err(e) => {
                eprintln!("fpython run: cannot execute: {}", e);
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprint!("{}", e);
            ExitCode::FAILURE
        }
    };
    let _ = std::fs::remove_dir_all(&dir);
    code
}

fn cmd_check(rest: &[String]) -> ExitCode {
    let mut ok = true;
    for f in rest {
        match driver::generate_c(Path::new(f)) {
            Ok(_) => eprintln!("{}: ok", f),
            Err(e) => {
                eprint!("{}", e.rendered);
                ok = false;
            }
        }
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Collect .fpy files (or .py when `py` is set) under the given paths.
pub fn collect_files(paths: &[String], exts: &[&str]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(p: &Path, exts: &[&str], out: &mut Vec<PathBuf>) {
        if p.is_dir() {
            if let Ok(rd) = std::fs::read_dir(p) {
                let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
                entries.sort();
                for e in entries {
                    let name = e.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if name.starts_with('.') || name == "target" || name == "__pycache__" || name == "node_modules" {
                        continue;
                    }
                    walk(&e, exts, out);
                }
            }
        } else if exts.iter().any(|x| p.extension().map(|e| e == *x).unwrap_or(false)) {
            out.push(p.to_path_buf());
        }
    }
    for p in paths {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            out.push(pb);
        } else {
            walk(&pb, exts, &mut out);
        }
    }
    out
}

fn cmd_fmt(rest: &[String]) -> ExitCode {
    let check = rest.iter().any(|a| a == "--check");
    let diff = rest.iter().any(|a| a == "--diff");
    let stdin = rest.iter().any(|a| a == "-");
    let paths: Vec<String> = rest.iter().filter(|a| !a.starts_with("--") && *a != "-").cloned().collect();
    if stdin {
        let mut src = String::new();
        use std::io::Read;
        let _ = std::io::stdin().read_to_string(&mut src);
        return match fmt::format_source(&src, "<stdin>") {
            Ok(out) => {
                print!("{}", out);
                ExitCode::SUCCESS
            }
            Err(d) => {
                eprint!("{}", d.render(Some(&src)));
                ExitCode::FAILURE
            }
        };
    }
    let paths = if paths.is_empty() { vec![".".to_string()] } else { paths };
    let files = collect_files(&paths, &["fpy"]);
    let mut changed = 0;
    let mut errors = 0;
    for f in &files {
        let src = match std::fs::read_to_string(f) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}: {}", f.display(), e);
                errors += 1;
                continue;
            }
        };
        match fmt::format_source(&src, &f.to_string_lossy()) {
            Ok(out) => {
                if out != src {
                    changed += 1;
                    if diff {
                        print!("{}", fmt::unified_diff(&src, &out, &f.to_string_lossy()));
                    } else if check {
                        println!("would reformat {}", f.display());
                    } else if let Err(e) = std::fs::write(f, &out) {
                        eprintln!("{}: {}", f.display(), e);
                        errors += 1;
                    } else {
                        println!("reformatted {}", f.display());
                    }
                }
            }
            Err(d) => {
                eprint!("{}", d.render(Some(&src)));
                errors += 1;
            }
        }
    }
    let verb = if check || diff { "would be reformatted" } else { "reformatted" };
    eprintln!("{} file(s) {}, {} unchanged{}", changed, verb, files.len() - changed - errors.min(files.len() - changed), if errors > 0 { format!(", {} error(s)", errors) } else { String::new() });
    if errors > 0 || (check && changed > 0) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Developer check: format files and verify reparse, idempotency and AST equivalence.
fn cmd_fmt_verify(rest: &[String]) -> ExitCode {
    let verbose = rest.iter().any(|a| a == "-v");
    let rest: Vec<String> = rest.iter().filter(|a| *a != "-v").cloned().collect();
    let files = collect_files(&rest, &["fpy", "py"]);
    let (mut ok, mut skipped, mut bad) = (0, 0, 0);
    for f in &files {
        let name = f.to_string_lossy().to_string();
        let Ok(src) = std::fs::read_to_string(f) else {
            skipped += 1;
            continue;
        };
        match fmt::verify(&src, &name, verbose, fmt::format_source) {
            fmt::Verify::Ok => ok += 1,
            fmt::Verify::Skipped => skipped += 1,
            fmt::Verify::Bad(report) => {
                print!("{}", report);
                bad += 1;
            }
        }
    }
    println!("ok={} bad={} skipped={}", ok, bad, skipped);
    if bad == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[allow(dead_code)]
fn fingerprint_file(p: &str) -> String {
    let src = std::fs::read_to_string(p).unwrap();
    fmt::ast_fingerprint(&parser::parse(&src, p).unwrap().module)
}

fn cmd_lint(rest: &[String]) -> ExitCode {
    let mut opts = lint::LintOptions::default();
    let mut paths = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--no-format" => opts.check_format = false,
            "--max-line" => {
                i += 1;
                opts.max_line = rest.get(i).and_then(|v| v.parse().ok()).unwrap_or(opts.max_line);
            }
            p => paths.push(p.to_string()),
        }
        i += 1;
    }
    if paths.is_empty() {
        paths.push(".".into());
    }
    let files = collect_files(&paths, &["fpy"]);
    let mut total = 0;
    for f in &files {
        let src = match std::fs::read_to_string(f) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}: {}", f.display(), e);
                total += 1;
                continue;
            }
        };
        for is in lint::lint_source(&src, &f.to_string_lossy(), &opts) {
            println!("{}:{}:{}: {} {}", f.display(), is.line, is.col, is.code, is.msg);
            total += 1;
        }
    }
    if total == 0 {
        eprintln!("{} file(s) checked, no issues", files.len());
        ExitCode::SUCCESS
    } else {
        eprintln!("{} issue(s) in {} file(s) checked", total, files.len());
        ExitCode::FAILURE
    }
}

fn cmd_convert(rest: &[String]) -> ExitCode {
    let mut outdir: Option<PathBuf> = None;
    let mut trace = false;
    let mut trace_args: Vec<String> = Vec::new();
    let mut check = true;
    let mut to_stdout = false;
    let mut paths = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "-o" | "--out" => {
                i += 1;
                outdir = rest.get(i).map(PathBuf::from);
            }
            "--trace" => trace = true,
            "--trace-args" => {
                i += 1;
                trace_args = rest.get(i).map(|s| s.split_whitespace().map(String::from).collect()).unwrap_or_default();
            }
            "--no-check" => check = false,
            "--stdout" => to_stdout = true,
            p => paths.push(p.to_string()),
        }
        i += 1;
    }
    if paths.is_empty() {
        eprintln!("usage: fpython convert <file.py|dir>... [-o outdir] [--trace] [--trace-args \"a b\"] [--no-check] [--stdout]");
        return ExitCode::from(2);
    }
    let files = collect_files(&paths, &["py"]);
    let mut written = Vec::new();
    let mut failures = 0;
    for f in &files {
        let src = match std::fs::read_to_string(f) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}: {}", f.display(), e);
                failures += 1;
                continue;
            }
        };
        let tr = if trace {
            match convert::run_trace(f, &trace_args) {
                Ok(t) => Some(t),
                Err(e) => {
                    eprintln!("{}: trace failed: {}", f.display(), e);
                    None
                }
            }
        } else {
            None
        };
        match convert::convert_source(&src, &f.to_string_lossy(), &convert::ConvertOptions { trace: tr }) {
            Ok((out, rep)) => {
                if to_stdout {
                    print!("{}", out);
                } else {
                    let dest = match &outdir {
                        Some(d) => {
                            let rel = paths.iter().find_map(|p| f.strip_prefix(p).ok()).filter(|r| !r.as_os_str().is_empty()).unwrap_or_else(|| Path::new(f.file_name().unwrap()));
                            d.join(rel).with_extension("fpy")
                        }
                        None => f.with_extension("fpy"),
                    };
                    if let Some(parent) = dest.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Err(e) = std::fs::write(&dest, &out) {
                        eprintln!("{}: {}", dest.display(), e);
                        failures += 1;
                        continue;
                    }
                    eprintln!(
                        "converted {} -> {} ({} params, {} returns annotated, {} Any, {} f-strings)",
                        f.display(),
                        dest.display(),
                        rep.annotated_params,
                        rep.annotated_returns,
                        rep.any_types,
                        rep.fstrings
                    );
                    written.push(dest);
                }
                for (line, note) in &rep.notes {
                    eprintln!("  {}:{}: review: {}", f.display(), line, note);
                }
            }
            Err(d) => {
                eprint!("{}", d.render(Some(&src)));
                failures += 1;
            }
        }
    }
    if check && !to_stdout {
        let mut clean = 0;
        for w in &written {
            match driver::generate_c(w) {
                Ok(_) => clean += 1,
                Err(e) => {
                    eprintln!("needs manual attention: {}", w.display());
                    eprint!("{}", e.rendered.lines().map(|l| format!("  {}\n", l)).collect::<String>());
                }
            }
        }
        eprintln!("{}/{} converted file(s) type-check cleanly", clean, written.len());
    }
    if failures > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn cmd_doc(rest: &[String]) -> ExitCode {
    let mut opts = doc::DocOptions { format: doc::Format::Html, private: false, title: String::new() };
    let (mut out, mut check, mut stdout) = (None, false, false);
    let mut paths: Vec<String> = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "-o" => {
                i += 1;
                out = rest.get(i).map(PathBuf::from);
            }
            "--format" => {
                i += 1;
                opts.format = match rest.get(i).map(|s| s.as_str()) {
                    Some("html") => doc::Format::Html,
                    Some("md") | Some("markdown") => doc::Format::Markdown,
                    other => {
                        eprintln!("fpython doc: unknown format {:?} (use html or md)", other.unwrap_or(""));
                        return ExitCode::from(2);
                    }
                };
            }
            "--title" => {
                i += 1;
                opts.title = rest.get(i).cloned().unwrap_or_default();
            }
            "--private" => opts.private = true,
            "--check" => check = true,
            "--stdout" => stdout = true,
            s if s.starts_with('-') => {
                eprintln!("fpython doc: unknown option '{}'", s);
                return ExitCode::from(2);
            }
            s => paths.push(s.to_string()),
        }
        i += 1;
    }
    // defaults: the project's source directory and name (fpython.toml), else the current directory
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let proj = project::find_project(&cwd).and_then(|p| p.ok());
    if paths.is_empty() {
        paths.push(proj.as_ref().and_then(|p| p.entry.parent().map(|d| d.to_string_lossy().to_string())).unwrap_or_else(|| ".".into()));
    }
    if opts.title.is_empty() {
        opts.title = proj.as_ref().map(|p| format!("{} API reference", p.name)).unwrap_or_else(|| "API reference".into());
    }
    // (root, file) pairs: modules are named relative to the directory given on the command line
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for p in &paths {
        let path = PathBuf::from(p);
        if path.is_dir() {
            for f in collect_files(&[p.clone()], &["fpy"]) {
                let in_tests = f.components().any(|c| c.as_os_str() == "tests" || c.as_os_str() == "build");
                let is_test = f.file_name().map(|n| n.to_string_lossy().starts_with("test_")).unwrap_or(false);
                if !in_tests && !is_test {
                    files.push((path.clone(), f));
                }
            }
        } else {
            files.push((path.parent().map(|d| d.to_path_buf()).unwrap_or_default(), path));
        }
    }
    if files.is_empty() {
        eprintln!("fpython doc: no .fpy files found");
        return ExitCode::FAILURE;
    }
    if check || stdout {
        let mut problems = 0;
        for (root, f) in &files {
            let src = match std::fs::read_to_string(f) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{}: {}", f.display(), e);
                    problems += 1;
                    continue;
                }
            };
            match doc::extract(&src, &f.to_string_lossy(), &doc::module_name(root, f), opts.private) {
                Ok(m) if check => {
                    for l in doc::missing_docs(&m) {
                        println!("{}", l);
                        problems += 1;
                    }
                }
                Ok(m) => println!("{}\n", doc::render_markdown(&m)),
                Err(d) => {
                    eprint!("{}", d.render(Some(&src)));
                    problems += 1;
                }
            }
        }
        if check {
            eprintln!("{} public item(s) without a docstring", problems);
        }
        return if problems > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS };
    }
    let out = out.unwrap_or_else(|| proj.as_ref().map(|p| p.root.join("build/docs")).unwrap_or_else(|| PathBuf::from("build/docs")));
    let report = doc::generate(&files, &out, &opts);
    for e in &report.errors {
        eprint!("{}", e);
    }
    let index = out.join(if opts.format == doc::Format::Html { "index.html" } else { "index.md" });
    eprintln!("documented {} module(s) in {}", report.written.len().saturating_sub(1), index.display());
    if report.errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn cmd_init(rest: &[String]) -> ExitCode {
    let name = match rest.first() {
        Some(n) => n.clone(),
        None => {
            eprintln!("usage: fpython init <name>");
            return ExitCode::from(2);
        }
    };
    let dir = PathBuf::from(&name);
    let pname = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(name.clone());
    match project::init_project(&dir, &pname) {
        Ok(files) => {
            for f in files {
                println!("created {}", f.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("fpython init: {}", e);
            ExitCode::FAILURE
        }
    }
}

/// `fpython test [paths]`: run `test_*` functions in `test_*.fpy` files (compiled natively).
fn cmd_test(rest: &[String]) -> ExitCode {
    let paths: Vec<String> = if rest.is_empty() { vec![".".into()] } else { rest.to_vec() };
    let files: Vec<PathBuf> = collect_files(&paths, &["fpy"])
        .into_iter()
        .filter(|f| f.file_name().map(|n| n.to_string_lossy().starts_with("test_")).unwrap_or(false))
        .collect();
    let (mut passed, mut failed) = (0, 0);
    for f in &files {
        let src = match std::fs::read_to_string(f) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{}: {}", f.display(), e);
                failed += 1;
                continue;
            }
        };
        let tests = match project::discover_tests(&src, &f.to_string_lossy()) {
            Ok(t) => t,
            Err(d) => {
                eprint!("{}", d.render(Some(&src)));
                failed += 1;
                continue;
            }
        };
        if tests.is_empty() {
            continue;
        }
        // compile a copy of the test module with a runner appended; sibling src/ modules are importable
        let dir = std::env::temp_dir().join(format!("fpython-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let runner = dir.join(f.file_name().unwrap());
        let _ = std::fs::write(&runner, format!("{}{}", src, project::test_runner_source(&tests)));
        let mut search = vec![f.parent().unwrap_or(Path::new(".")).to_path_buf()];
        if let Some(Ok(p)) = project::find_project(f.parent().unwrap_or(Path::new("."))) {
            search.push(p.entry.parent().unwrap_or(&p.root).to_path_buf());
        }
        for d in &search {
            if let Ok(rd) = std::fs::read_dir(d) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.extension().map(|x| x == "fpy").unwrap_or(false) && p.file_name() != f.file_name() {
                        let _ = std::fs::copy(&p, dir.join(p.file_name().unwrap()));
                    }
                }
            }
        }
        let exe = dir.join("test_bin");
        let opts = driver::BuildOpts { output: Some(exe.clone()), ..Default::default() };
        match driver::build(&runner, &opts) {
            Ok(_) => {
                let out = Command::new(&exe).output();
                match out {
                    Ok(o) => {
                        let text = String::from_utf8_lossy(&o.stdout).to_string();
                        for line in text.lines() {
                            if line.starts_with("PASS ") {
                                passed += 1;
                                println!("{} {}", f.display(), line);
                            } else if line.starts_with("FAIL ") {
                                failed += 1;
                                println!("{} {}", f.display(), line);
                            }
                        }
                        if !o.status.success() && !text.contains("FAIL ") {
                            failed += 1;
                            println!("{} ERROR: {}", f.display(), String::from_utf8_lossy(&o.stderr).trim());
                        }
                    }
                    Err(e) => {
                        failed += 1;
                        eprintln!("{}: {}", f.display(), e);
                    }
                }
            }
            Err(e) => {
                failed += 1;
                eprint!("{}", e.replace(&runner.to_string_lossy().to_string(), &f.to_string_lossy()));
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    println!("{} passed, {} failed", passed, failed);
    if failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn cmd_bundle(rest: &[String]) -> ExitCode {
    if rest.len() != 2 {
        eprintln!("usage: fpython bundle <executable> <rootfs-dir>");
        return ExitCode::from(2);
    }
    let py = driver::python_exe().unwrap_or_else(|| "python3".into());
    match bundle::bundle(Path::new(&rest[0]), Path::new(&rest[1]), &py) {
        Ok(r) => {
            eprintln!("bundled {} file(s), {} shared librar(ies) into {}", r.files, r.libs, rest[1]);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("fpython bundle: {}", e);
            ExitCode::FAILURE
        }
    }
}
