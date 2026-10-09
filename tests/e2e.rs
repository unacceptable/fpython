//! End-to-end tests: drive the `fpython` binary like a user would.
//!
//! * tests/run/*.fpy     — compiled and run; stdout must equal the CPython golden `.out`
//! * tests/errors/*.fpy  — must fail to compile with the message given in `# expect:`
//! * tests/py/*.fpy      — CPython-fallback interop (fastapi, fastmcp, requests, httpx, boto3, unittest)
//! * tests/convert/*.py  — `fpython convert` output must type-check and behave like the original
//! * CLI behaviour of fmt / lint / check / build / version
//!
//! Docker tests (FROM scratch image) run only when FPY_DOCKER_TESTS=1.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fpython"))
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).current_dir(root()).output().expect("failed to run fpython")
}

fn files(dir: &str, ext: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(root().join(dir)).unwrap().filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().map(|x| x == ext).unwrap_or(false)).collect();
    v.sort();
    v
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fpy-e2e-{}-{}", name, std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn golden_programs_match_cpython() {
    let mut failures = Vec::new();
    for f in files("tests/run", "fpy") {
        let expected = std::fs::read_to_string(f.with_extension("out")).unwrap();
        let out = run(&["run", f.to_str().unwrap()]);
        let got = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        if got != expected {
            failures.push(format!("{}:\n--- expected\n{}\n--- got\n{}", f.display(), expected, got));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn compile_errors_are_reported() {
    let mut failures = Vec::new();
    for f in files("tests/errors", "fpy") {
        let src = std::fs::read_to_string(&f).unwrap();
        let expect = src.lines().next().unwrap().trim_start_matches("# expect:").trim().to_string();
        let out = run(&["check", f.to_str().unwrap()]);
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        if out.status.success() || !err.contains(&expect) {
            failures.push(format!("{}: expected error containing {:?}, got:\n{}", f.display(), expect, err));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn python_has(module: &str) -> bool {
    Command::new("python3").args(["-c", &format!("import {}", module)]).output().map(|o| o.status.success()).unwrap_or(false)
}

#[test]
fn python_library_interop() {
    let needs = [
        ("requests_basic", "urllib"),
        ("unittest_basic", "unittest"),
        ("http_clients", "requests"),
        ("boto3_basic", "boto3"),
        ("fastapi_app", "fastapi"),
        ("fastmcp_server", "fastmcp"),
    ];
    let mut failures = Vec::new();
    for (name, module) in needs {
        if !python_has(module) {
            eprintln!("skipping {} (python module {} not installed)", name, module);
            continue;
        }
        let f = root().join("tests/py").join(format!("{}.fpy", name));
        let expected = std::fs::read_to_string(f.with_extension("out")).unwrap();
        let out = run(&["run", f.to_str().unwrap()]);
        let got = String::from_utf8_lossy(&out.stdout).to_string();
        if !out.status.success() || got != expected {
            failures.push(format!("{} (exit {:?}):\n--- expected\n{}\n--- got\n{}\n--- stderr\n{}", name, out.status.code(), expected, got, String::from_utf8_lossy(&out.stderr)));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

/// Pure-Python modules imported by an fpython program are compiled to native code; with fallback
/// disabled, any construct the Python->C compiler can't handle fails the test.
#[test]
fn native_python_module_compilation() {
    if !python_has("asyncio") {
        return;
    }
    let mut failures = Vec::new();
    // a fresh cache: the runtime and every module are compiled (not just cache hits)
    let cache = tmpdir("native-cache");
    let _ = std::fs::remove_dir_all(&cache);
    for f in files("tests/py", "fpy").into_iter().filter(|f| f.file_name().unwrap().to_string_lossy().starts_with("native_")) {
        let expected = std::fs::read_to_string(f.with_extension("out")).unwrap();
        // direct mode compiles the imported modules; deep mode whole packages (verbose report on)
        for mode in ["direct", "deep"] {
            let out = Command::new(bin()).args(["run", f.to_str().unwrap()]).env("FPY_NO_FALLBACK", "1").env("FPY_NATIVE_PY", mode).env("FPY_CACHE_DIR", &cache).env("FPY_VERBOSE", "1").current_dir(root()).output().unwrap();
            let got = String::from_utf8_lossy(&out.stdout);
            if !out.status.success() || got != expected {
                failures.push(format!("{} ({}):\n--- got\n{}\n--- stderr\n{}", f.display(), mode, got, String::from_utf8_lossy(&out.stderr)));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn convert_python_program() {
    let dir = tmpdir("convert");
    let src = root().join("tests/convert/inventory.py");
    let dst = dir.join("inventory.py");
    std::fs::copy(&src, &dst).unwrap();
    let out = run(&["convert", dst.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("1/1 converted file(s) type-check cleanly"), "{}", err);
    let fpy = dir.join("inventory.fpy");
    let converted = std::fs::read_to_string(&fpy).unwrap();
    assert!(converted.contains("def word_counts(text: str) -> dict[str, int]:"), "{}", converted);
    let py = Command::new("python3").arg(&src).output().unwrap();
    let native = run(&["run", fpy.to_str().unwrap()]);
    assert_eq!(String::from_utf8_lossy(&native.stdout), String::from_utf8_lossy(&py.stdout));
    // --stdout mode and trace mode
    let o2 = run(&["convert", "--stdout", "--no-check", src.to_str().unwrap()]);
    assert!(String::from_utf8_lossy(&o2.stdout).contains("class Inventory:"));
    let traced = root().join("tests/convert/traced.py");
    let o3 = run(&["convert", "--stdout", "--no-check", "--trace", traced.to_str().unwrap()]);
    assert!(String::from_utf8_lossy(&o3.stdout).contains("def summarize(record: dict[str, Any]) -> str:"), "{}", String::from_utf8_lossy(&o3.stdout));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fmt_cli() {
    let dir = tmpdir("fmt");
    let f = dir.join("messy.fpy");
    std::fs::write(&f, "def f(x:int,y:int=2)->int:\n  return x+y\nprint( f(1) )\n").unwrap();
    let check = run(&["fmt", "--check", f.to_str().unwrap()]);
    assert!(!check.status.success(), "fmt --check should fail on unformatted input");
    let fix = run(&["fmt", f.to_str().unwrap()]);
    assert!(fix.status.success());
    let s = std::fs::read_to_string(&f).unwrap();
    assert_eq!(s, "def f(x: int, y: int = 2) -> int:\n    return x + y\n\n\nprint(f(1))\n");
    assert!(run(&["fmt", "--check", f.to_str().unwrap()]).status.success());
    // stdin mode
    let mut child = Command::new(bin()).args(["fmt", "-"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(b"x=[1,2,3]\n").unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout), "x = [1, 2, 3]\n");
    // a syntax error is reported
    std::fs::write(dir.join("bad.fpy"), "def (:\n").unwrap();
    assert!(!run(&["fmt", dir.join("bad.fpy").to_str().unwrap()]).status.success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fmt_is_stable_on_repository_sources() {
    // every .fpy file in the repository is kept formatted (except the deliberate syntax error)
    let out = run(&["fmt", "--check", "stdlib", "tests/run", "tests/py", "examples", "bench"]);
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let errs: Vec<String> = files("tests/errors", "fpy").into_iter().filter(|f| !f.ends_with("syntax.fpy")).map(|f| f.to_string_lossy().into_owned()).collect();
    let mut args = vec!["fmt", "--check"];
    args.extend(errs.iter().map(|s| s.as_str()));
    let out = run(&args);
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

#[test]
fn lint_cli() {
    let dir = tmpdir("lint");
    let f = dir.join("l.fpy");
    std::fs::write(&f, "import os\n\n\ndef f(x) -> None:\n    if x == None:\n        pass\n").unwrap();
    let out = run(&["lint", f.to_str().unwrap()]);
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    for code in ["F401", "T001", "B003"] {
        assert!(s.contains(code), "missing {} in {}", code, s);
    }
    std::fs::write(&f, "def f(x: int) -> int:\n    return x\n").unwrap();
    let ok = run(&["lint", f.to_str().unwrap()]);
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stdout));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_check_and_misc_cli() {
    let dir = tmpdir("build");
    let exe = dir.join("hello_bin");
    let out = run(&["build", "--release", "tests/run/hello.fpy", "-o", exe.to_str().unwrap(), "--emit-c"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(exe.with_extension("c").exists(), "--emit-c keeps the C file");
    let o = Command::new(&exe).output().unwrap();
    assert!(String::from_utf8_lossy(&o.stdout).starts_with("Hello, fpython!"));
    assert!(run(&["check", "tests/run/classes.fpy"]).status.success());
    assert!(String::from_utf8_lossy(&run(&["version"]).stdout).starts_with("fpython "));
    assert!(String::from_utf8_lossy(&run(&["help"]).stdout).contains("USAGE"));
    assert!(!run(&["nonsense-command"]).status.success());
    assert!(!run(&["build"]).status.success());
    // program arguments and exit codes
    let p = dir.join("args.fpy");
    std::fs::write(&p, "import sys\nprint(sys.argv[1:])\nsys.exit(3)\n").unwrap();
    let o = run(&["run", p.to_str().unwrap(), "a", "b"]);
    assert_eq!(String::from_utf8_lossy(&o.stdout), "['a', 'b']\n");
    assert_eq!(o.status.code(), Some(3));
    // uncaught exception: exit 1 with the message on stderr
    std::fs::write(&p, "raise ValueError('boom')\n").unwrap();
    let o = run(&["run", p.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("ValueError: boom"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn docker_from_scratch_image() {
    if std::env::var("FPY_DOCKER_TESTS").is_err() {
        eprintln!("skipping docker test (set FPY_DOCKER_TESTS=1)");
        return;
    }
    let r = root();
    let b = Command::new("docker").args(["build", "-q", "-f", "docker/Dockerfile", "-t", "fpython:latest", "."]).current_dir(&r).status().unwrap();
    assert!(b.success());
    let b2 = Command::new("docker").args(["build", "-q", "-t", "fpython-hello", "examples/hello-docker"]).current_dir(&r).status().unwrap();
    assert!(b2.success());
    let o = Command::new("docker").args(["run", "--rm", "fpython-hello", "1000"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), r#"{"limit": 1000, "count": 168, "largest": 997, "platform": "linux"}"#);
    // the final image contains nothing but the static binary
    let hist = Command::new("docker").args(["image", "inspect", "fpython-hello", "--format", "{{.Size}}"]).output().unwrap();
    let size: u64 = String::from_utf8_lossy(&hist.stdout).trim().parse().unwrap_or(u64::MAX);
    assert!(size < 5_000_000, "scratch image unexpectedly large: {}", size);
}

#[allow(dead_code)]
fn exists(p: &Path) -> bool {
    p.exists()
}
