//! Build driver: compile .fpy -> C -> native executable.

use crate::compiler::{Compiler, Options};
use crate::diag::Diag;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const RUNTIME_FILES: &[(&str, &str)] = &[
    ("fpy.h", include_str!("../runtime/fpy.h")),
    ("gc.c", include_str!("../runtime/gc.c")),
    ("str.c", include_str!("../runtime/str.c")),
    ("containers.c", include_str!("../runtime/containers.c")),
    ("core.c", include_str!("../runtime/core.c")),
    ("gen.c", include_str!("../runtime/gen.c")),
    ("pybridge.c", include_str!("../runtime/pybridge.c")),
    ("pyc.c", include_str!("../runtime/pyc.c")),
    ("fpy_pyc_api.h", include_str!("../runtime/fpy_pyc_api.h")),
];

#[derive(Debug, Clone)]
pub struct BuildOpts {
    pub output: Option<PathBuf>,
    pub opt_level: String,
    pub static_link: bool,
    pub cc: String,
    pub target: Option<String>,
    pub emit_c: bool,
    pub debug: bool,
    pub extra_cflags: Vec<String>,
}

impl Default for BuildOpts {
    fn default() -> Self {
        BuildOpts {
            output: None,
            opt_level: "-O2".into(),
            static_link: false,
            cc: std::env::var("FPY_CC").or_else(|_| std::env::var("CC")).unwrap_or_else(|_| "cc".into()),
            target: None,
            emit_c: false,
            debug: false,
            extra_cflags: Vec::new(),
        }
    }
}

pub fn hash_str(s: &str) -> u64 {
    let mut h: u64 = 1469598103934665603;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// Write the embedded runtime into a cache directory and return it.
pub fn runtime_dir() -> std::io::Result<PathBuf> {
    let mut all = String::new();
    for (n, s) in RUNTIME_FILES {
        all.push_str(n);
        all.push_str(s);
    }
    let base = std::env::var_os("FPY_CACHE_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache").join("fpython")))
        .unwrap_or_else(|| std::env::temp_dir().join("fpython-cache"));
    let dir = base.join(format!("rt-{:016x}", hash_str(&all)));
    if !dir.join("fpy_runtime.c").exists() {
        std::fs::create_dir_all(&dir)?;
        for (n, s) in RUNTIME_FILES {
            std::fs::write(dir.join(n), s)?;
        }
        let mut umbrella = String::from("/* fpython runtime umbrella */\n");
        for (n, _) in RUNTIME_FILES.iter().skip(1).filter(|(n, _)| n.ends_with(".c")) {
            umbrella.push_str(&format!("#include \"{}\"\n", n));
        }
        std::fs::write(dir.join("fpy_runtime.c"), umbrella)?;
    }
    Ok(dir)
}

pub struct CompileError {
    pub diag: Diag,
    pub rendered: String,
}

pub fn python_exe() -> Option<String> {
    if std::env::var("FPY_NO_PYTHON").is_ok() {
        return None;
    }
    Some(std::env::var("FPY_PYTHON").unwrap_or_else(|_| "python3".into()))
}

pub struct Generated {
    pub code: String,
    pub uses_python: bool,
    /// Natively compiled Python modules: (name, C source) compiled as separate cached objects.
    pub py_modules: Vec<(String, String)>,
}

/// Type-check and generate C for a program.
pub fn generate_c(entry: &Path) -> Result<String, CompileError> {
    generate(entry).map(|g| g.code)
}

pub fn generate(entry: &Path) -> Result<Generated, CompileError> {
    let root = entry.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
    let opts = Options { project_root: root, python: python_exe() };
    let mut c = Compiler::new(opts);
    match c.compile_program(entry) {
        Ok(code) => {
            let mut code = code;
            let mut py_modules = Vec::new();
            if c.uses_python {
                // Natively compile pure-Python dependencies; the rest is imported by CPython.
                let (registry, mods, report) = compile_python_deps(&c.py_imported, entry);
                code = code.replace("#ifdef FPY_PYTHON\n#endif\n", &format!("#ifdef FPY_PYTHON\n{}#endif\n", registry));
                py_modules = mods;
                if std::env::var("FPY_VERBOSE").is_ok() {
                    for line in report {
                        eprintln!("{}", line);
                    }
                }
            }
            Ok(Generated { code, uses_python: c.uses_python, py_modules })
        }
        Err(d) => {
            let src = c.source_of(&d.file).map(|s| s.to_string()).or_else(|| std::fs::read_to_string(&d.file).ok());
            let rendered = d.render(src.as_deref());
            Err(CompileError { diag: d, rendered })
        }
    }
}

/// Compiler/linker flags for embedding CPython 3.14 (CPython fallback).
pub fn python_flags() -> Result<(Vec<String>, Vec<String>), String> {
    let py = python_exe().ok_or("Python fallback disabled (FPY_NO_PYTHON)")?;
    let q = |code: &str| -> Result<String, String> {
        let o = Command::new(&py).args(["-c", code]).output().map_err(|e| format!("cannot run {}: {}", py, e))?;
        if !o.status.success() {
            return Err(String::from_utf8_lossy(&o.stderr).to_string());
        }
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let include = q("import sysconfig; print(sysconfig.get_path('include'))")?;
    let prefix = q("import sys; print(sys.base_prefix)")?;
    let libdir = q("import sysconfig; print(sysconfig.get_config_var('LIBDIR') or '')")?;
    let ldver = q("import sysconfig; print(sysconfig.get_config_var('LDVERSION') or sysconfig.get_python_version())")?;
    let libs = q("import sysconfig; print(sysconfig.get_config_var('LIBS') or '', sysconfig.get_config_var('SYSLIBS') or '')")?;
    let cflags = vec![
        "-DFPY_PYTHON".to_string(),
        format!("-I{}", include),
        format!("-DFPY_PYTHONHOME=\"{}\"", prefix),
    ];
    let mut ldflags = vec![format!("-L{}", libdir), format!("-lpython{}", ldver), format!("-Wl,-rpath,{}", libdir)];
    for l in libs.split_whitespace() {
        ldflags.push(l.to_string());
    }
    Ok((cflags, ldflags))
}

/// Native-compilation policy for Python dependencies: FPY_NATIVE_PY=none|direct|deep (default direct).
fn compile_python_deps(names: &std::collections::BTreeSet<String>, entry: &Path) -> (String, Vec<(String, String)>, Vec<String>) {
    let mode = std::env::var("FPY_NATIVE_PY").unwrap_or_else(|_| "direct".into());
    let mut report = Vec::new();
    let mut mods = Vec::new();
    if mode != "none" {
        let py = python_exe().unwrap_or_else(|| "python3".into());
        let entry_dir = entry.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."));
        // ask Python where each module lives (origin, package dir) and whether it is stdlib
        let script = "import sys, importlib.util, sysconfig, os\nsys.path.insert(0, sys.argv[1])\nstd = os.path.realpath(sysconfig.get_path('stdlib'))\nsite = [os.path.realpath(sysconfig.get_path(k)) for k in ('purelib', 'platlib')]\nfor n in sys.argv[2:]:\n    try:\n        s = importlib.util.find_spec(n)\n    except Exception:\n        s = None\n    if s is None or not s.origin or not s.origin.endswith('.py'):\n        print(n, '-', '-', 'ext'); continue\n    o = os.path.realpath(s.origin)\n    pkg = s.submodule_search_locations[0] if s.submodule_search_locations else '-'\n    print(n, o, pkg, 'std' if o.startswith(std + os.sep) and not any(o.startswith(x + os.sep) for x in site) else 'lib')\n";
        let mut args = vec!["-c".to_string(), script.to_string(), entry_dir.to_string_lossy().to_string()];
        args.extend(names.iter().cloned());
        let out = Command::new(&py).args(&args).output();
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        let mut todo: Vec<(String, PathBuf)> = Vec::new();
        for line in text.lines() {
            let parts: Vec<&str> = line.splitn(4, ' ').collect();
            if parts.len() < 4 {
                continue;
            }
            let (name, origin, pkg, kind) = (parts[0], parts[1], parts[2], parts[3]);
            if kind == "ext" {
                report.push(format!("python module {}: CPython (extension/builtin)", name));
                continue;
            }
            if kind == "std" && std::env::var("FPY_NATIVE_STDLIB").is_err() {
                report.push(format!("python module {}: CPython (stdlib)", name));
                continue;
            }
            todo.push((name.to_string(), PathBuf::from(origin)));
            if mode == "deep" && pkg != "-" {
                walk_package(Path::new(pkg), name, &mut todo);
            }
        }
        let mut seen = std::collections::HashSet::new();
        todo.retain(|(n, _)| seen.insert(n.clone()));
        for (name, path) in todo {
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            match crate::pycomp::compile_module(&name, &path.to_string_lossy(), &src) {
                Ok(m) => {
                    report.push(format!("python module {}: compiled natively", name));
                    mods.push(m);
                }
                Err(why) => report.push(format!("python module {}: CPython fallback ({})", name, why)),
            }
        }
    }
    let registry = crate::pycomp::registry(&mods);
    let sources = mods.into_iter().map(|m| (m.name, format!("#include \"fpy_pyc_api.h\"\n{}", m.c_code))).collect();
    (registry, sources, report)
}

/// Compile natively generated Python modules to object files (cached, in parallel).
fn compile_py_objects(mods: &[(String, String)], cc: &str, cflags: &[String], rt: &Path, opt: &str) -> Result<Vec<PathBuf>, String> {
    let cache = rt.parent().unwrap_or(rt).join("pyc");
    std::fs::create_dir_all(&cache).map_err(|e| e.to_string())?;
    let flags_key = format!("{}{}{}", cc, opt, cflags.join(" "));
    let jobs: Vec<(String, String, PathBuf)> = mods
        .iter()
        .map(|(n, src)| {
            let h = hash_str(&format!("{}{}{}", flags_key, n, src));
            (n.clone(), src.clone(), cache.join(format!("{}-{:016x}.o", crate::compiler::c_ident(n), h)))
        })
        .collect();
    let todo: Vec<&(String, String, PathBuf)> = jobs.iter().filter(|(_, _, o)| !o.exists()).collect();
    let nthreads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).max(1);
    let errors = std::sync::Mutex::new(Vec::new());
    let next = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|sc| {
        for _ in 0..nthreads.min(todo.len().max(1)) {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if i >= todo.len() {
                    break;
                }
                let (name, src, obj) = todo[i];
                let cfile = obj.with_extension(format!("{}.c", std::process::id()));
                let tmp_o = obj.with_extension(format!("{}.o.tmp", std::process::id()));
                if let Err(e) = std::fs::write(&cfile, src) {
                    errors.lock().unwrap().push(format!("{}: {}", name, e));
                    continue;
                }
                let mut cmd = Command::new(cc);
                cmd.arg("-std=gnu11").arg(opt).arg("-w").arg("-fno-strict-aliasing").arg("-c").arg(format!("-I{}", rt.display()));
                for f in cflags {
                    cmd.arg(f);
                }
                cmd.arg(&cfile).arg("-o").arg(&tmp_o);
                match cmd.output() {
                    Ok(o) if o.status.success() => {
                        let _ = std::fs::rename(&tmp_o, obj);
                    }
                    Ok(o) => errors.lock().unwrap().push(format!("{}: {}", name, String::from_utf8_lossy(&o.stderr))),
                    Err(e) => errors.lock().unwrap().push(format!("{}: {}", name, e)),
                }
                let _ = std::fs::remove_file(&cfile);
            });
        }
    });
    let errs = errors.into_inner().unwrap();
    if !errs.is_empty() {
        return Err(format!("C compilation of native Python modules failed (this is a bug in fpython):\n{}", errs.join("\n")));
    }
    Ok(jobs.into_iter().map(|(_, _, o)| o).collect())
}

fn walk_package(dir: &Path, pkg: &str, out: &mut Vec<(String, PathBuf)>) {
    let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().map(|e| e.path()).collect(),
        Err(_) => return,
    };
    entries.sort();
    for p in entries {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if p.is_dir() && p.join("__init__.py").exists() && !name.starts_with('_') && name != "tests" {
            let sub = format!("{}.{}", pkg, name);
            out.push((sub.clone(), p.join("__init__.py")));
            walk_package(&p, &sub, out);
        } else if name.ends_with(".py") && name != "__init__.py" && name != "__main__.py" {
            out.push((format!("{}.{}", pkg, name.trim_end_matches(".py")), p));
        }
    }
}

pub fn build(entry: &Path, opts: &BuildOpts) -> Result<PathBuf, String> {
    let gen = generate(entry).map_err(|e| e.rendered)?;
    let code = gen.code.clone();
    let out = opts.output.clone().unwrap_or_else(|| {
        let stem = entry.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "a.out".into());
        entry.with_file_name(stem)
    });
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {}", parent.display(), e))?;
        }
    }
    let rt = runtime_dir().map_err(|e| format!("cannot write runtime: {}", e))?;
    let cfile = if opts.emit_c {
        out.with_extension("c")
    } else {
        let d = std::env::temp_dir().join(format!("fpython-{}", std::process::id()));
        std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        d.join(format!("{}.c", out.file_name().unwrap().to_string_lossy()))
    };
    std::fs::write(&cfile, &code).map_err(|e| format!("cannot write {}: {}", cfile.display(), e))?;
    let mut cmd = Command::new(&opts.cc);
    cmd.arg("-std=gnu11").arg(&opts.opt_level).arg("-w").arg("-fno-strict-aliasing").arg("-fwrapv");
    if opts.debug {
        cmd.arg("-g");
    }
    if let Some(t) = &opts.target {
        cmd.arg(format!("--target={}", t));
    }
    let mut py_ld = Vec::new();
    let mut py_objs = Vec::new();
    if gen.uses_python {
        let (cf, lf) = python_flags()?;
        let opt = if opts.opt_level == "-O0" { "-O0" } else { "-O2" };
        py_objs = compile_py_objects(&gen.py_modules, &opts.cc, &cf, &rt, opt)?;
        for f in cf {
            cmd.arg(f);
        }
        py_ld = lf;
    }
    cmd.arg(format!("-I{}", rt.display())).arg(&cfile).arg("-o").arg(&out);
    if opts.static_link {
        cmd.arg("-static");
    }
    for f in &opts.extra_cflags {
        cmd.arg(f);
    }
    for o in &py_objs {
        cmd.arg(o);
    }
    for f in py_ld {
        cmd.arg(f);
    }
    cmd.arg("-lm");
    let res = cmd.output().map_err(|e| format!("cannot run C compiler '{}': {}", opts.cc, e))?;
    if !opts.emit_c {
        let _ = std::fs::remove_file(&cfile);
    }
    if !res.status.success() {
        return Err(format!(
            "C compilation failed (this is a bug in fpython; please report it)\n{}{}",
            String::from_utf8_lossy(&res.stdout),
            String::from_utf8_lossy(&res.stderr)
        ));
    }
    Ok(out)
}
