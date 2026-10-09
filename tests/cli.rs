//! CLI end-to-end tests: every `fpython` subcommand, its options, and its error paths.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_fpython"))
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(bin()).args(args).current_dir(dir).output().expect("failed to run fpython")
}

fn run(args: &[&str]) -> Output {
    run_in(&root(), args)
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fpy-cli-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// `fpython init`, then `fpython test` and a project build that reads fpython.toml.
#[test]
fn init_test_and_project_build() {
    let dir = tmpdir("proj");
    assert_eq!(run_in(&dir, &["init"]).status.code(), Some(2));
    let o = run_in(&dir, &["init", "demo"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).contains("created demo/fpython.toml"));
    assert!(!run_in(&dir, &["init", "demo"]).status.success(), "init refuses to overwrite a project");
    let proj = dir.join("demo");

    let o = run_in(&proj, &["test"]);
    assert!(o.status.success(), "{}{}", stdout(&o), stderr(&o));
    assert!(stdout(&o).contains("1 passed, 0 failed"), "{}", stdout(&o));

    // a failing test, a crashing test file, and a test file that doesn't compile
    std::fs::write(proj.join("tests/test_more.fpy"), "def test_bad() -> None:\n    assert 1 == 2, \"nope\"\n\n\ndef test_ok() -> None:\n    pass\n").unwrap();
    std::fs::write(proj.join("tests/test_broken.fpy"), "def test_x() -> None:\n    return 1\n").unwrap();
    std::fs::write(proj.join("tests/test_syntax.fpy"), "def test_x(:\n").unwrap();
    std::fs::write(proj.join("tests/test_empty.fpy"), "x = 1\n").unwrap();
    let o = run_in(&proj, &["test"]);
    assert!(!o.status.success());
    let out = stdout(&o);
    assert!(out.contains("FAIL test_bad: AssertionError: nope"), "{}", out);
    assert!(out.contains("PASS test_ok") || out.contains("3 passed"), "{}", out);

    // `fpython build` with no file uses fpython.toml (release = true, output build/demo)
    let o = run_in(&proj, &["build"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let exe = proj.join("build/demo");
    assert!(exe.exists());
    assert_eq!(stdout(&Command::new(&exe).arg("cli").output().unwrap()), "Hello, cli!\n");
    std::fs::write(proj.join("fpython.toml"), "[project]\nname = \"demo\"\n[build]\nstatic = true\noutput = \"out/app\"\n").unwrap();
    let _ = run_in(&proj, &["build", "--debug"]);
    std::fs::write(proj.join("fpython.toml"), "[project\n").unwrap();
    assert!(!run_in(&proj, &["build"]).status.success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn build_and_run_options() {
    let dir = tmpdir("opts");
    let p = dir.join("prog.fpy");
    std::fs::write(&p, "import sys\nprint(sys.argv[1:])\n").unwrap();
    let ps = p.to_str().unwrap();
    // `--` separates program arguments, and a dash argument after the file belongs to the program
    let o = run(&["run", "--debug", ps, "--", "-x", "y"]);
    assert_eq!(stdout(&o), "['-x', 'y']\n", "{}", stderr(&o));
    let o = run(&["run", ps, "-v"]);
    assert_eq!(stdout(&o), "['-v']\n");
    let o = run(&["run", "--cc", "cc", ps]);
    assert!(o.status.success(), "{}", stderr(&o));
    // option errors
    for args in [vec!["run", "--bogus", ps], vec!["build", "-o"], vec!["build", "--target"], vec!["build", "--cc"], vec!["run"]] {
        let o = run(&args);
        assert!(!o.status.success(), "{:?} should fail", args);
    }
    // a bad cross target or compiler fails cleanly
    let o = run(&["build", ps, "--cc", "/nonexistent/cc", "-o", dir.join("x").to_str().unwrap()]);
    assert!(!o.status.success());
    let o = run(&["build", ps, "--target", "nonsense-triple", "-o", dir.join("y").to_str().unwrap()]);
    assert!(!o.status.success());
    // compile errors are reported by run and build
    std::fs::write(&p, "x: int = 'no'\n").unwrap();
    assert!(stderr(&run(&["run", ps])).contains("type mismatch"));
    assert!(!run(&["build", ps]).status.success());
    assert!(!run(&["check", ps, "tests/run/hello.fpy"]).status.success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fmt_options_and_errors() {
    let dir = tmpdir("fmt");
    let f = dir.join("a.fpy");
    std::fs::write(&f, "x=1\n").unwrap();
    let fs = f.to_str().unwrap();
    let o = run(&["fmt", "--diff", fs]);
    assert!(stdout(&o).contains("-x=1\n+x = 1\n"), "{}", stdout(&o));
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "x=1\n", "--diff doesn't write");
    assert!(!run(&["fmt", "--check", fs]).status.success());
    assert!(run(&["fmt", fs]).status.success());
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "x = 1\n");
    assert!(run(&["fmt", "--check", fs]).status.success());
    // stdin with a syntax error
    let mut child = Command::new(bin()).args(["fmt", "-"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    use std::io::Write;
    child.stdin.take().unwrap().write_all(b"def (:\n").unwrap();
    assert!(!child.wait_with_output().unwrap().status.success());
    // unreadable (non-UTF-8) file
    std::fs::write(dir.join("bin.fpy"), [0xff, 0xfe, 0x00]).unwrap();
    assert!(!run(&["fmt", dir.to_str().unwrap()]).status.success());
    // with no paths, fmt formats the current directory
    std::fs::remove_file(dir.join("bin.fpy")).unwrap();
    std::fs::write(&f, "y=2\n").unwrap();
    assert!(run_in(&dir, &["fmt"]).status.success());
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "y = 2\n");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hidden_developer_commands() {
    let dir = tmpdir("dev");
    let good = dir.join("good.py");
    std::fs::write(&good, "# comment\ndef f(a, b=1):\n    return [a, b]  # trailing\n").unwrap();
    std::fs::write(dir.join("bad.py"), "def (:\n").unwrap();
    std::fs::write(dir.join("bin.py"), [0xff, 0xfe]).unwrap();
    let o = run(&["fmt-verify", "-v", dir.to_str().unwrap()]);
    assert!(stdout(&o).contains("ok=1 bad=0 skipped=2"), "{}", stdout(&o));
    assert!(o.status.success());
    let o = run(&["ast-fingerprint", good.to_str().unwrap()]);
    assert!(stdout(&o).contains("FuncDef"), "{}", stdout(&o));
    let o = run(&["parse", good.to_str().unwrap(), dir.join("bad.py").to_str().unwrap()]);
    assert!(stderr(&o).contains("1 failures / 2"), "{}", stderr(&o));
    let o = run(&["pycomp", "good", good.to_str().unwrap()]);
    assert!(stdout(&o).contains("pym_good"), "{}", stderr(&o));
    std::fs::write(&good, "def f():\n    return locals()\n").unwrap();
    assert!(!run(&["pycomp", "good", good.to_str().unwrap()]).status.success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn lint_options() {
    let dir = tmpdir("lint");
    let f = dir.join("l.fpy");
    std::fs::write(&f, format!("x = \"{}\"\n", "a".repeat(100))).unwrap();
    assert!(!run(&["lint", f.to_str().unwrap()]).status.success());
    assert!(run(&["lint", "--max-line", "200", f.to_str().unwrap()]).status.success());
    assert!(run(&["lint", "--no-format", "--max-line", "200", f.to_str().unwrap()]).status.success());
    // the current directory is the default; unreadable files are reported
    std::fs::write(dir.join("bin.fpy"), [0xff, 0xfe]).unwrap();
    assert!(!run_in(&dir, &["lint"]).status.success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn convert_options() {
    let dir = tmpdir("convert");
    let src = dir.join("src");
    std::fs::create_dir_all(src.join("pkg")).unwrap();
    std::fs::write(src.join("pkg/m.py"), "def double(x):\n    return x * 2\n\n\nprint(double(21))\n").unwrap();
    std::fs::write(src.join("dyn.py"), "import json\n\n\ndef load(s):\n    return json.loads(s)['k'].upper()\n\n\nprint(load('{\"k\": \"v\"}'))\n").unwrap();
    std::fs::write(src.join("bad.py"), "def (:\n").unwrap();
    assert_eq!(run(&["convert"]).status.code(), Some(2));
    let out = dir.join("out");
    let o = run(&["convert", src.to_str().unwrap(), "-o", out.to_str().unwrap(), "--trace", "--trace-args", "a b"]);
    assert!(!o.status.success(), "the syntax error is a failure");
    assert!(out.join("pkg/m.fpy").exists(), "directory layout is kept under -o: {}", stderr(&o));
    // --stdout prints instead of writing, --no-check skips type checking
    let o = run(&["convert", src.join("pkg/m.py").to_str().unwrap(), "--stdout", "--no-check"]);
    assert!(stdout(&o).contains("def double(x: int) -> int:"), "{}", stdout(&o));
    assert!(!src.join("pkg/m.fpy").exists());
    // a program whose trace run fails still converts statically
    std::fs::write(src.join("crash.py"), "def f(a):\n    return a + 1\n\n\nraise SystemExit(f(1) + undefined_name)\n").unwrap();
    let o = run(&["convert", src.join("crash.py").to_str().unwrap(), "--trace", "--stdout"]);
    assert!(stdout(&o).contains("def f(a: int) -> int:"), "{}{}", stdout(&o), stderr(&o));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn bundle_usage_and_errors() {
    assert_eq!(run(&["bundle"]).status.code(), Some(2));
    let dir = tmpdir("bundle");
    let o = run(&["bundle", dir.join("missing").to_str().unwrap(), dir.join("rootfs").to_str().unwrap()]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("fpython bundle:"), "{}", stderr(&o));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn doc_generation() {
    let dir = tmpdir("doc");
    std::fs::create_dir_all(dir.join("src/pkg")).unwrap();
    std::fs::write(dir.join("fpython.toml"), "[project]\nname = \"geo\"\nentry = \"src/main.fpy\"\n").unwrap();
    std::fs::write(dir.join("src/main.fpy"), "\"\"\"Entry point.\"\"\"\n\n\ndef main() -> None:\n    \"\"\"Run it.\"\"\"\n    pass\n").unwrap();
    std::fs::write(dir.join("src/pkg/__init__.fpy"), "\"\"\"The package.\"\"\"\n").unwrap();
    std::fs::write(dir.join("src/pkg/shapes.fpy"), "class Square:\n    side: float = 1.0\n\n    def area(self) -> float:\n        return self.side * self.side\n").unwrap();
    std::fs::write(dir.join("src/test_main.fpy"), "def test_x() -> None:\n    pass\n").unwrap();
    // defaults: the project's src/ into build/docs, titled after the project
    let o = run_in(&dir, &["doc"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let index = std::fs::read_to_string(dir.join("build/docs/index.html")).unwrap();
    assert!(index.contains("<h1>geo API reference</h1>") && index.contains("pkg.shapes.html"), "{}", index);
    assert!(!index.contains("test_main"), "test files are skipped");
    // markdown, explicit output and title, private names
    let md = dir.join("md");
    assert!(run_in(&dir, &["doc", "src", "--format", "md", "-o", md.to_str().unwrap(), "--title", "Geo", "--private"]).status.success());
    assert!(std::fs::read_to_string(md.join("index.md")).unwrap().starts_with("# Geo\n"));
    assert!(std::fs::read_to_string(md.join("pkg.shapes.md")).unwrap().contains("def area(self) -> float"));
    // --stdout and --check
    let o = run_in(&dir, &["doc", "src/pkg/shapes.fpy", "--stdout"]);
    assert!(stdout(&o).starts_with("# `shapes`"), "{}", stdout(&o));
    let o = run_in(&dir, &["doc", "--check"]);
    assert!(!o.status.success());
    assert!(stdout(&o).contains("class 'Square' has no docstring") && stdout(&o).contains("method 'Square.area' has no docstring"), "{}", stdout(&o));
    assert!(run_in(&dir, &["doc", "src/main.fpy", "--check"]).status.success());
    // errors
    assert_eq!(run_in(&dir, &["doc", "--format", "pdf"]).status.code(), Some(2));
    assert_eq!(run_in(&dir, &["doc", "--bogus"]).status.code(), Some(2));
    std::fs::write(dir.join("bad.fpy"), "def (:\n").unwrap();
    assert!(!run_in(&dir, &["doc", "bad.fpy", "--stdout"]).status.success());
    assert!(!run_in(&dir, &["doc", "bad.fpy", "-o", dir.join("out").to_str().unwrap()]).status.success());
    assert!(!run_in(&dir, &["doc", "missing.fpy", "--check"]).status.success());
    let _ = std::fs::remove_dir_all(&dir);
    // no project and no .fpy files
    let empty = tmpdir("doc-empty");
    assert!(!run_in(&empty, &["doc"]).status.success());
    let _ = std::fs::remove_dir_all(&empty);
}
