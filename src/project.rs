//! Project support: `fpython.toml`, `fpython init`, and `fpython test`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Minimal TOML subset: `[section]` headers and `key = value` (strings, booleans, integers).
pub fn parse_toml(src: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let mut section = String::new();
    for (i, raw) in src.lines().enumerate() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if !line.ends_with(']') {
                return Err(format!("line {}: malformed section header", i + 1));
            }
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        let (k, v) = line.split_once('=').ok_or_else(|| format!("line {}: expected key = value", i + 1))?;
        let key = if section.is_empty() { k.trim().to_string() } else { format!("{}.{}", section, k.trim()) };
        let v = v.trim();
        let value = if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
            v[1..v.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
        } else if v == "true" || v == "false" || v.parse::<i64>().is_ok() {
            v.to_string()
        } else {
            return Err(format!("line {}: unsupported value {:?}", i + 1, v));
        };
        out.insert(key, value);
    }
    Ok(out)
}

fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

#[derive(Debug, Clone, Default)]
pub struct Project {
    pub root: PathBuf,
    pub name: String,
    pub entry: PathBuf,
    pub release: bool,
    pub static_link: bool,
    pub output: Option<PathBuf>,
}

/// Find and load `fpython.toml` in `dir` or its ancestors.
pub fn find_project(dir: &Path) -> Option<Result<Project, String>> {
    let mut cur = Some(dir.to_path_buf());
    while let Some(d) = cur {
        let f = d.join("fpython.toml");
        if f.is_file() {
            return Some(load_project(&f));
        }
        cur = d.parent().map(|p| p.to_path_buf());
    }
    None
}

pub fn load_project(file: &Path) -> Result<Project, String> {
    let src = std::fs::read_to_string(file).map_err(|e| format!("{}: {}", file.display(), e))?;
    let t = parse_toml(&src).map_err(|e| format!("{}: {}", file.display(), e))?;
    let root = file.parent().unwrap_or(Path::new(".")).to_path_buf();
    let name = t.get("project.name").cloned().ok_or_else(|| format!("{}: missing project.name", file.display()))?;
    let entry = root.join(t.get("project.entry").cloned().unwrap_or_else(|| "src/main.fpy".into()));
    Ok(Project {
        output: t.get("build.output").map(|o| root.join(o)),
        release: t.get("build.release").map(|v| v == "true").unwrap_or(false),
        static_link: t.get("build.static").map(|v| v == "true").unwrap_or(false),
        root,
        name,
        entry,
    })
}

pub fn init_project(dir: &Path, name: &str) -> Result<Vec<PathBuf>, String> {
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') || name.is_empty() {
        return Err(format!("invalid project name '{}'", name));
    }
    if dir.join("fpython.toml").exists() {
        return Err(format!("{} already contains fpython.toml", dir.display()));
    }
    let files: Vec<(&str, String)> = vec![
        ("fpython.toml", format!("[project]\nname = \"{name}\"\nversion = \"0.1.0\"\nentry = \"src/main.fpy\"\n\n[build]\nrelease = true\nstatic = false\n")),
        (
            "src/main.fpy",
            "\"\"\"Entry point.\"\"\"\nimport sys\n\n\ndef greet(name: str) -> str:\n    return f\"Hello, {name}!\"\n\n\ndef main() -> None:\n    name = sys.argv[1] if len(sys.argv) > 1 else \"world\"\n    print(greet(name))\n\n\nif __name__ == \"__main__\":\n    main()\n"
                .to_string(),
        ),
        (
            "tests/test_main.fpy",
            "from main import greet\n\n\ndef test_greet() -> None:\n    assert greet(\"fpython\") == \"Hello, fpython!\"\n".to_string(),
        ),
        (
            "Dockerfile",
            format!(
                "# Build with the fpython toolchain image, ship FROM scratch.\nFROM fpython:latest AS build\nWORKDIR /src\nCOPY . .\nRUN fpython build --release --static src/main.fpy -o /out/{name}\n\nFROM scratch\nCOPY --from=build /out/{name} /{name}\nENTRYPOINT [\"/{name}\"]\n"
            ),
        ),
        (".gitignore", "/build/\n*.c\n".to_string()),
        ("README.md", format!("# {name}\n\nAn [fpython](https://fpython.org) project.\n\n```sh\nfpython run src/main.fpy\nfpython test\nfpython build\n```\n")),
    ];
    let mut created = Vec::new();
    for (rel, content) in files {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&p, content).map_err(|e| format!("{}: {}", p.display(), e))?;
        created.push(p);
    }
    Ok(created)
}

/// Names of top-level `def test_*() -> None` functions in a test file.
pub fn discover_tests(src: &str, file: &str) -> Result<Vec<String>, crate::diag::Diag> {
    let parsed = crate::parser::parse(src, file)?;
    let mut out = Vec::new();
    for st in &parsed.module.body {
        if let crate::ast::StmtKind::FuncDef(f) = &st.kind {
            if f.name.starts_with("test") && f.params.all_positional().next().is_none() {
                out.push(f.name.clone());
            }
        }
    }
    Ok(out)
}

/// Source of a runner appended to a test module: runs each test, reports, and sets the exit code.
pub fn test_runner_source(tests: &[String]) -> String {
    let mut s = String::from("\n\ndef __fpy_run_tests() -> None:\n    import sys\n    failed = 0\n");
    for t in tests {
        s.push_str(&format!(
            "    try:\n        {t}()\n        print(\"PASS {t}\")\n    except BaseException as e:\n        failed += 1\n        print(f\"FAIL {t}: {{type(e).__name__}}: {{e}}\")\n"
        ));
    }
    s.push_str("    sys.exit(1 if failed else 0)\n\n\n__fpy_run_tests()\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_parsing() {
        let t = parse_toml("# c\nname = \"x\" # trailing\n[build]\nrelease = true\njobs = 4\n").unwrap();
        assert_eq!(t["name"], "x");
        assert_eq!(t["build.release"], "true");
        assert_eq!(t["build.jobs"], "4");
        assert!(parse_toml("[bad\n").is_err());
        assert!(parse_toml("novalue\n").is_err());
        assert!(parse_toml("x = [1]\n").is_err());
        assert_eq!(parse_toml("s = \"a # b\"\n").unwrap()["s"], "a # b");
    }

    #[test]
    fn init_and_load() {
        let d = std::env::temp_dir().join(format!("fpy-init-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let files = init_project(&d, "demo").unwrap();
        assert!(files.iter().any(|f| f.ends_with("fpython.toml")));
        assert!(init_project(&d, "demo").is_err());
        assert!(init_project(&d, "bad name").is_err());
        let p = find_project(&d.join("src")).unwrap().unwrap();
        assert_eq!(p.name, "demo");
        assert!(p.release);
        assert!(!p.static_link);
        assert!(p.entry.ends_with("src/main.fpy"));
        std::fs::write(d.join("fpython.toml"), "[project]\n").unwrap();
        assert!(load_project(&d.join("fpython.toml")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn test_discovery() {
        let t = discover_tests("def test_a() -> None:\n    pass\n\n\ndef helper(x: int) -> None:\n    pass\n\n\ndef test_b() -> None:\n    pass\n", "t.fpy").unwrap();
        assert_eq!(t, vec!["test_a", "test_b"]);
        let r = test_runner_source(&t);
        assert!(r.contains("test_a()") && r.contains("sys.exit"));
        assert!(discover_tests("def (", "t.fpy").is_err());
    }
}
