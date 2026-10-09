//! fpython type checker + C code generator.
//!
//! Checking and lowering happen in one pass over each function body. Generic functions and
//! classes are monomorphized on demand. The program is emitted as a single C translation unit
//! that includes the runtime, so the C compiler can inline runtime fast paths everywhere.

pub mod scope;
mod call;
mod expr;
mod func;
pub use func::always_terminates;
pub use expr::float_lit as float_lit_pub;

use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::parser;
use crate::types::*;
use scope::Scope;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub type R<T> = Result<T, Diag>;

pub static STDLIB: &[(&str, &str)] = &[
    ("builtins", include_str!("../../stdlib/builtins.fpy")),
    ("math", include_str!("../../stdlib/math.fpy")),
    ("sys", include_str!("../../stdlib/sys.fpy")),
    ("os", include_str!("../../stdlib/os.fpy")),
    ("os.path", include_str!("../../stdlib/os/path.fpy")),
    ("time", include_str!("../../stdlib/time.fpy")),
    ("random", include_str!("../../stdlib/random.fpy")),
    ("string", include_str!("../../stdlib/string.fpy")),
    ("json", include_str!("../../stdlib/json.fpy")),
    ("functools", include_str!("../../stdlib/functools.fpy")),
    ("itertools", include_str!("../../stdlib/itertools.fpy")),
    ("collections", include_str!("../../stdlib/collections.fpy")),
    ("heapq", include_str!("../../stdlib/heapq.fpy")),
    ("bisect", include_str!("../../stdlib/bisect.fpy")),
];

/// Modules whose names are understood by the compiler itself.
const VIRTUAL_MODULES: &[&str] =
    &["typing", "typing_extensions", "collections.abc", "dataclasses", "abc", "__future__", "fpython", "enum"];

#[derive(Debug, Clone)]
pub enum Sym {
    Global(usize),
    Func(usize),
    Class(ClassId),
    /// Generic class template (index into class_templates).
    ClassTemplate(usize),
    Module(usize),
    Alias(Ty),
    /// Compiler-known name (typing constructs, builtin functions).
    Builtin(String),
}

pub struct ModInfo {
    pub name: String,
    pub path: String,
    pub src: Rc<String>,
    pub ast: Rc<Module>,
    pub scope: HashMap<String, Sym>,
    pub init_cname: String,
    pub is_builtins: bool,
    pub declared: bool,
    /// module-level global types inferred (or in progress)
    pub inferred: bool,
}

#[derive(Debug, Clone)]
pub struct GlobalVar {
    pub name: String,
    pub cname: String,
    pub ty: Option<Ty>,
    pub module: usize,
    pub decl_span: Span,
    /// Set for names bound by importing a Python module (CPython fallback).
    pub py_import: Option<PyImport>,
}

#[derive(Debug, Clone)]
pub struct PyImport {
    pub module: String,
    pub attr: Option<String>,
    pub top_level: bool,
}

/// Marker returned by find_module for modules provided by CPython.
pub const PY_MODULE: usize = usize::MAX - 1;

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: Ty,
    pub default: Option<Expr>,
    pub owner: ClassId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FnKind {
    Plain,
    Method,
    Static,
    ClassMethod,
    Property,
}

pub struct ClassInfo {
    pub name: String,
    pub cname: String,
    pub module: usize,
    pub def: Rc<ClassDef>,
    pub base: Option<ClassId>,
    pub fields: Vec<Field>,
    /// Own methods by name.
    pub methods: Vec<(String, usize)>,
    pub subst: HashMap<String, Ty>,
    pub is_dataclass: bool,
    pub dc_order: bool,
    pub dc_frozen: bool,
    pub is_enum: bool,
    pub enum_members: Vec<(String, i64)>,
    pub subclasses: Vec<ClassId>,
    pub resolved: bool,
    pub class_vars: Vec<(String, usize)>,
    pub span: Span,
    pub is_protocol: bool,
}

pub struct ClassTemplate {
    pub name: String,
    pub module: usize,
    pub def: Rc<ClassDef>,
    pub instances: HashMap<Vec<Ty>, ClassId>,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Ty,
    pub default: Option<Expr>,
    pub kwonly: bool,
}

#[derive(Debug, Clone)]
pub struct Sig {
    pub params: Vec<Param>,
    pub vararg: Option<(String, Ty)>,
    pub kwarg: Option<(String, Ty)>,
    pub ret: Ty,
}

pub struct FuncInfo {
    pub name: String,
    pub qualname: String,
    pub module: usize,
    pub def: Rc<FuncDef>,
    pub owner: Option<ClassId>,
    pub kind: FnKind,
    pub subst: HashMap<String, Ty>,
    pub sig: Option<Sig>,
    pub extern_name: Option<String>,
    pub instances: HashMap<Vec<Ty>, String>,
    pub scope: Option<Rc<Scope>>,
    /// Methods that override a base method use the base's signature for vtable dispatch.
    pub overrides: Option<usize>,
    /// Method of a Python-derived class: first parameter is untyped (Any).
    pub py_self: bool,
    /// For `async def`: the declared result type (callers get an awaitable `Any`).
    pub async_ret: Option<Ty>,
    /// Global holding the Python class object (for super()).
    pub py_class: Option<usize>,
    pub cname_hint: Option<String>,
}

pub struct PyClassInfo {
    pub module: usize,
    pub def: Rc<ClassDef>,
    pub global: usize,
    pub methods: Vec<(String, usize, Option<String>)>,
}

pub struct Job {
    pub func: usize,
    pub targs: Vec<Ty>,
    pub cname: String,
}

#[derive(Default)]
pub struct Out {
    pub structs: String,
    pub protos: String,
    pub funcs: String,
    pub types: String,
    pub strings: String,
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    pub project_root: PathBuf,
    /// Python 3.14 interpreter used to resolve modules for the CPython fallback (None disables it).
    pub python: Option<String>,
}

pub struct Compiler {
    pub opts: Options,
    pub modules: Vec<ModInfo>,
    pub mod_index: HashMap<String, usize>,
    pub classes: Vec<ClassInfo>,
    pub templates: Vec<ClassTemplate>,
    pub funcs: Vec<FuncInfo>,
    pub globals: Vec<GlobalVar>,
    pub queue: VecDeque<Job>,
    pub out: Out,
    pub strings: HashMap<String, String>,
    pub tmp: usize,
    pub fx_stack: Vec<func::Fx>,
    pub builtins_mod: usize,
    pub value_adapters: HashMap<String, String>,
    pub dyn_wrappers: HashMap<String, String>,
    pub emitted_types: HashSet<ClassId>,
    pub lambda_count: usize,
    pub tuple_kinds: HashMap<Vec<u8>, String>,
    /// typed adapters for callables arriving as Any, by signature
    pub func_adapters: HashMap<String, String>,
    pub uses_python: bool,
    py_spec_cache: HashMap<String, bool>,
    pub py_classes: Vec<PyClassInfo>,
    /// Python modules imported through the CPython fallback (candidates for native compilation).
    pub py_imported: BTreeSet<String>,
    /// C code appended to the program (natively compiled Python modules).
    pub extra_c: String,
}

pub fn c_ident(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect()
}

pub fn c_string(s: &[u8]) -> String {
    let mut out = String::from("\"");
    let mut prev_hex = false;
    for &b in s {
        let esc = match b {
            b'"' => Some("\\\"".to_string()),
            b'\\' => Some("\\\\".to_string()),
            b'\n' => Some("\\n".to_string()),
            b'\r' => Some("\\r".to_string()),
            b'\t' => Some("\\t".to_string()),
            b'?' => Some("\\?".to_string()),
            0x20..=0x7e => None,
            _ => Some(format!("\\{:03o}", b)),
        };
        match esc {
            Some(e) => out.push_str(&e),
            None => {
                let _ = prev_hex;
                out.push(b as char)
            }
        }
        prev_hex = false;
    }
    out.push('"');
    out
}

impl Compiler {
    pub fn new(opts: Options) -> Compiler {
        Compiler {
            opts,
            modules: Vec::new(),
            mod_index: HashMap::new(),
            classes: Vec::new(),
            templates: Vec::new(),
            funcs: Vec::new(),
            globals: Vec::new(),
            queue: VecDeque::new(),
            out: Out::default(),
            strings: HashMap::new(),
            tmp: 0,
            fx_stack: Vec::new(),
            builtins_mod: 0,
            value_adapters: HashMap::new(),
            dyn_wrappers: HashMap::new(),
            emitted_types: HashSet::new(),
            lambda_count: 0,
            tuple_kinds: HashMap::new(),
            func_adapters: HashMap::new(),
            uses_python: false,
            py_spec_cache: HashMap::new(),
            py_classes: Vec::new(),
            py_imported: BTreeSet::new(),
            extra_c: String::new(),
        }
    }

    pub fn fresh(&mut self, prefix: &str) -> String {
        self.tmp += 1;
        format!("_{}{}", prefix, self.tmp)
    }

    pub fn class_cname(&self, id: ClassId) -> String {
        self.classes[id].cname.clone()
    }

    pub fn ctype(&self, t: &Ty) -> String {
        let names: Vec<String> = self.classes.iter().map(|c| c.cname.clone()).collect();
        t.ctype(&|id| names[id].clone())
    }

    pub fn tyname(&self, t: &Ty) -> String {
        let names: Vec<String> = self.classes.iter().map(|c| c.name.clone()).collect();
        TyDisplay { ty: t, names: &|id| names[id].clone() }.to_string()
    }

    pub fn err(&self, module: usize, span: Span, msg: impl Into<String>) -> Diag {
        let path = self.modules.get(module).map(|m| m.path.clone()).unwrap_or_default();
        Diag::new(&path, span, msg)
    }

    pub fn source_of(&self, file: &str) -> Option<Rc<String>> {
        self.modules.iter().find(|m| m.path == file).map(|m| m.src.clone())
    }

    // ------------------------------------------------------------------ module loading

    pub fn load_module_src(&mut self, name: &str, path: &str, src: String) -> R<usize> {
        if let Some(&id) = self.mod_index.get(name) {
            return Ok(id);
        }
        let parsed = parser::parse(&src, path)?;
        let id = self.modules.len();
        self.modules.push(ModInfo {
            name: name.to_string(),
            path: path.to_string(),
            src: Rc::new(src),
            ast: Rc::new(parsed.module),
            scope: HashMap::new(),
            init_cname: format!("mod_init_{}", c_ident(name)),
            is_builtins: name == "builtins",
            declared: false,
            inferred: false,
        });
        self.mod_index.insert(name.to_string(), id);
        Ok(id)
    }

    fn find_module(&mut self, name: &str, from: usize, span: Span) -> R<Option<usize>> {
        if let Some(&id) = self.mod_index.get(name) {
            return Ok(Some(id));
        }
        if VIRTUAL_MODULES.contains(&name) {
            return Ok(None);
        }
        let rel: PathBuf = name.split('.').collect();
        let mut dirs = vec![self.opts.project_root.clone()];
        if let Some(parent) = Path::new(&self.modules[from].path).parent() {
            dirs.insert(0, parent.to_path_buf());
        }
        for d in &dirs {
            for cand in [d.join(&rel).with_extension("fpy"), d.join(&rel).join("__init__.fpy")] {
                if cand.is_file() {
                    let src = std::fs::read_to_string(&cand).map_err(|e| self.err(from, span, format!("cannot read {}: {}", cand.display(), e)))?;
                    let id = self.load_module_src(name, &cand.to_string_lossy(), src)?;
                    self.declare_module(id)?;
                    return Ok(Some(id));
                }
            }
        }
        if let Some((_, src)) = STDLIB.iter().find(|(n, _)| *n == name) {
            let id = self.load_module_src(name, &format!("<stdlib>/{}.fpy", name.replace('.', "/")), src.to_string())?;
            self.declare_module(id)?;
            return Ok(Some(id));
        }
        // Not an fpython module: fall back to CPython 3.14 if it can import it.
        if self.python_importable(name) {
            self.uses_python = true;
            self.py_imported.insert(name.to_string());
            return Ok(Some(PY_MODULE));
        }
        Err(self.err(from, span, format!("no module named '{}'", name)))
    }

    /// Does this expression refer to a Python (CPython fallback) object, e.g. `BaseModel` or `unittest.TestCase`?
    pub fn is_python_expr(&self, m: usize, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Name(n) => matches!(self.lookup_sym(m, n), Some(Sym::Global(g)) if self.globals[g].py_import.is_some() || (self.globals[g].ty == Some(Ty::Any) && self.py_classes.iter().any(|c| c.global == g))),
            ExprKind::Attribute { value, .. } | ExprKind::Subscript { value, .. } | ExprKind::Call { func: value, .. } => self.is_python_expr(m, value),
            _ => false,
        }
    }

    pub fn python_importable(&mut self, name: &str) -> bool {
        if let Some(&b) = self.py_spec_cache.get(name) {
            return b;
        }
        let py = match &self.opts.python {
            Some(p) => p.clone(),
            None => return false,
        };
        let root = self.opts.project_root.to_string_lossy().to_string();
        let out = std::process::Command::new(&py)
            .args(["-c", "import importlib.util, sys\nsys.path.insert(0, sys.argv[2])\ntry:\n    print(importlib.util.find_spec(sys.argv[1]) is not None)\nexcept Exception:\n    print(False)", name, &root])
            .output();
        let ok = matches!(out, Ok(o) if o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "True");
        self.py_spec_cache.insert(name.to_string(), ok);
        ok
    }

    fn py_global(&mut self, m: usize, bind: &str, imp: PyImport, span: Span) -> Sym {
        if let Some(Sym::Global(g)) = self.modules[m].scope.get(bind).cloned() {
            if self.globals[g].ty == Some(Ty::Any) {
                self.globals[g].py_import = Some(imp);
                return Sym::Global(g);
            }
        }
        let gid = self.globals.len();
        let cname = format!("g_{}_py_{}", c_ident(&self.modules[m].name), c_ident(bind));
        self.globals.push(GlobalVar { name: bind.to_string(), cname, ty: Some(Ty::Any), module: m, decl_span: span, py_import: Some(imp) });
        Sym::Global(gid)
    }

    /// Bind import statements of a module (and nested function-level imports are handled at use).
    fn process_import(&mut self, m: usize, st: &Stmt, scope_out: &mut Vec<(String, Sym)>) -> R<()> {
        match &st.kind {
            StmtKind::Import(names) => {
                for a in names {
                    let target = self.find_module(&a.name, m, st.span)?;
                    if target == Some(PY_MODULE) {
                        let (bind, imp) = match &a.asname {
                            Some(asn) => (asn.clone(), PyImport { module: a.name.clone(), attr: None, top_level: false }),
                            None => (a.name.split('.').next().unwrap().to_string(), PyImport { module: a.name.clone(), attr: None, top_level: true }),
                        };
                        let sym = self.py_global(m, &bind, imp, st.span);
                        scope_out.push((bind, sym));
                        continue;
                    }
                    match (&a.asname, target) {
                        (Some(asn), Some(id)) => scope_out.push((asn.clone(), Sym::Module(id))),
                        (None, Some(_)) => {
                            // `import a.b` binds `a`; make sure parent packages exist as modules.
                            let top = a.name.split('.').next().unwrap().to_string();
                            let top_id = self.find_module(&top, m, st.span)?;
                            if let Some(t) = top_id {
                                scope_out.push((top, Sym::Module(t)));
                            }
                        }
                        (asn, None) => {
                            let n = asn.clone().unwrap_or_else(|| a.name.clone());
                            scope_out.push((n, Sym::Builtin(format!("module:{}", a.name))));
                        }
                    }
                }
            }
            StmtKind::ImportFrom { module, level, names } => {
                let modname = self.resolve_relative(m, *level, module);
                let target = self.find_module(&modname, m, st.span)?;
                for a in names {
                    let bind = a.asname.clone().unwrap_or_else(|| a.name.clone());
                    if target == Some(PY_MODULE) {
                        if a.name == "*" {
                            return Err(self.err(m, st.span, format!("'from {} import *' is not supported for Python modules", modname)));
                        }
                        let sym = self.py_global(m, &bind, PyImport { module: modname.clone(), attr: Some(a.name.clone()), top_level: false }, st.span);
                        scope_out.push((bind, sym));
                        continue;
                    }
                    match target {
                        None => scope_out.push((bind, Sym::Builtin(a.name.clone()))),
                        Some(id) => {
                            if a.name == "*" {
                                let syms: Vec<(String, Sym)> = self.modules[id]
                                    .scope
                                    .iter()
                                    .filter(|(k, _)| !k.starts_with('_'))
                                    .map(|(k, v)| (k.clone(), v.clone()))
                                    .collect();
                                scope_out.extend(syms);
                                continue;
                            }
                            let sym = match self.modules[id].scope.get(&a.name) {
                                Some(s) => s.clone(),
                                None => {
                                    // maybe a submodule
                                    let sub = format!("{}.{}", modname, a.name);
                                    match self.find_module(&sub, m, st.span) {
                                        Ok(Some(sid)) => Sym::Module(sid),
                                        _ => {
                                            return Err(self.err(m, st.span, format!("cannot import name '{}' from '{}'", a.name, modname)))
                                        }
                                    }
                                }
                            };
                            scope_out.push((bind, sym));
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Absolute name of `from <dots><module> import ...` in module `m`. In a package's
    /// `__init__`, one dot means the package itself.
    pub fn resolve_relative(&self, m: usize, level: usize, module: &str) -> String {
        if level == 0 {
            return module.to_string();
        }
        let is_pkg = self.modules[m].path.ends_with("__init__.fpy");
        let mut parts: Vec<&str> = self.modules[m].name.split('.').collect();
        for _ in 0..(if is_pkg { level - 1 } else { level }) {
            parts.pop();
        }
        if !module.is_empty() {
            parts.push(module);
        }
        parts.join(".")
    }

    /// Load the native modules named by imports nested in functions, classes or compound
    /// statements, so they are declared and initialized like top-level imports (their names are
    /// bound when the import statement itself is compiled).
    fn preload_nested_imports(&mut self, m: usize, body: &[Stmt]) -> R<()> {
        fn collect<'a>(body: &'a [Stmt], top: bool, out: &mut Vec<&'a Stmt>) {
            for st in body {
                match &st.kind {
                    StmtKind::Import(_) | StmtKind::ImportFrom { .. } if !top => out.push(st),
                    StmtKind::Import(_) | StmtKind::ImportFrom { .. } => {}
                    StmtKind::FuncDef(f) => collect(&f.body, false, out),
                    StmtKind::ClassDef(c) => collect(&c.body, false, out),
                    _ => scope::walk_stmt(st, &mut |n| {
                        if let scope::Node::B(b) = n {
                            collect(b, false, out)
                        }
                    }),
                }
            }
        }
        let mut found = Vec::new();
        collect(body, true, &mut found);
        for st in found {
            let names: Vec<String> = match &st.kind {
                StmtKind::Import(names) => names.iter().map(|a| a.name.clone()).collect(),
                StmtKind::ImportFrom { module, level, .. } => vec![self.resolve_relative(m, *level, module)],
                _ => vec![],
            };
            for n in names {
                let parts: Vec<&str> = n.split('.').collect();
                for i in 1..=parts.len() {
                    self.find_module(&parts[..i].join("."), m, st.span)?;
                }
            }
        }
        Ok(())
    }

    /// Declaration pass: register classes, functions and globals of a module.
    pub fn declare_module(&mut self, m: usize) -> R<()> {
        if self.modules[m].declared {
            return Ok(());
        }
        self.modules[m].declared = true;
        let ast = self.modules[m].ast.clone();
        self.preload_nested_imports(m, &ast.body)?;
        let modc = c_ident(&self.modules[m].name);
        let mut binds: Vec<(String, Sym)> = Vec::new();
        for st in &ast.body {
            match &st.kind {
                StmtKind::Import(_) | StmtKind::ImportFrom { .. } => {
                    self.process_import(m, st, &mut binds)?;
                    for (k, v) in binds.drain(..) {
                        self.modules[m].scope.insert(k, v);
                    }
                }
                StmtKind::FuncDef(fd) => {
                    let id = self.new_func(m, Rc::new((**fd).clone()), None, FnKind::Plain, HashMap::new(), format!("f_{}_{}", modc, c_ident(&fd.name)));
                    self.modules[m].scope.insert(fd.name.clone(), Sym::Func(id));
                }
                StmtKind::ClassDef(cd) if cd.bases.iter().any(|b| self.is_python_expr(m, b)) => {
                    // Class deriving from a Python class: becomes a real Python class at runtime.
                    self.uses_python = true;
                    let gid = self.globals.len();
                    let cname = format!("g_{}_pycls_{}", modc, c_ident(&cd.name));
                    self.globals.push(GlobalVar { name: cd.name.clone(), cname, ty: Some(Ty::Any), module: m, decl_span: st.span, py_import: None });
                    self.modules[m].scope.insert(cd.name.clone(), Sym::Global(gid));
                    let mut methods = Vec::new();
                    for s2 in &cd.body {
                        if let StmtKind::FuncDef(fd) = &s2.kind {
                            let mut wrap = None;
                            let mut first_is_self = true;
                            for d in &fd.decorators {
                                if let ExprKind::Name(n) = &d.kind {
                                    match n.as_str() {
                                        "staticmethod" => {
                                            wrap = Some("staticmethod".to_string());
                                            first_is_self = false;
                                        }
                                        "classmethod" => wrap = Some("classmethod".to_string()),
                                        "property" => wrap = Some("property".to_string()),
                                        _ => {}
                                    }
                                }
                            }
                            let hint = format!("f_{}_{}_{}", modc, c_ident(&cd.name), c_ident(&fd.name));
                            let fid = self.new_func(m, Rc::new((**fd).clone()), None, FnKind::Plain, HashMap::new(), String::new());
                            self.funcs[fid].py_self = first_is_self;
                            self.funcs[fid].py_class = Some(gid);
                            self.funcs[fid].cname_hint = Some(hint);
                            self.funcs[fid].qualname = format!("{}.{}", cd.name, fd.name);
                            methods.push((fd.name.clone(), fid, wrap));
                        }
                    }
                    self.py_classes.push(PyClassInfo { module: m, def: Rc::new((**cd).clone()), global: gid, methods });
                }
                StmtKind::ClassDef(cd) => {
                    if !cd.type_params.is_empty() {
                        let tid = self.templates.len();
                        self.templates.push(ClassTemplate { name: cd.name.clone(), module: m, def: Rc::new((**cd).clone()), instances: HashMap::new() });
                        self.modules[m].scope.insert(cd.name.clone(), Sym::ClassTemplate(tid));
                    } else {
                        let id = self.new_class(m, Rc::new((**cd).clone()), HashMap::new(), &[], st.span);
                        self.modules[m].scope.insert(cd.name.clone(), Sym::Class(id));
                    }
                }
                StmtKind::TypeAlias { .. } => {}
                _ => {
                    let mut names = BTreeSet::new();
                    collect_assigned(st, &mut names);
                    for n in names {
                        if !self.modules[m].scope.contains_key(&n) || matches!(self.modules[m].scope.get(&n), Some(Sym::Builtin(_))) {
                            let gid = self.globals.len();
                            self.globals.push(GlobalVar { name: n.clone(), cname: format!("g_{}_{}", modc, c_ident(&n)), ty: None, module: m, decl_span: st.span, py_import: None });
                            self.modules[m].scope.insert(n, Sym::Global(gid));
                        }
                    }
                }
            }
        }
        // Globals created inside functions (`global x` then `x = ...`), typed from a literal value.
        for st in &ast.body {
            let fds: Vec<&FuncDef> = match &st.kind {
                StmtKind::FuncDef(fd) => vec![&**fd],
                StmtKind::ClassDef(cd) => cd.body.iter().filter_map(|s| if let StmtKind::FuncDef(fd) = &s.kind { Some(&**fd) } else { None }).collect(),
                _ => vec![],
            };
            for fd in fds {
                let sc = scope::analyze_function(&fd.params, &fd.body);
                for n in &sc.globals {
                    if self.modules[m].scope.contains_key(n) && !matches!(self.modules[m].scope.get(n), Some(Sym::Builtin(_))) {
                        continue;
                    }
                    let ty = global_literal_type(&fd.body, n);
                    let gid = self.globals.len();
                    self.globals.push(GlobalVar { name: n.clone(), cname: format!("g_{}_{}", modc, c_ident(n)), ty, module: m, decl_span: st.span, py_import: None });
                    self.modules[m].scope.insert(n.clone(), Sym::Global(gid));
                }
            }
        }
        // Type aliases and module-level annotated globals after classes are known.
        for st in &ast.body {
            match &st.kind {
                StmtKind::TypeAlias { name, value, .. } => {
                    let t = self.resolve_ann(value, m, &HashMap::new())?;
                    self.modules[m].scope.insert(name.clone(), Sym::Alias(t));
                }
                StmtKind::Assign { targets, value } if targets.len() == 1 => {
                    // `X = int | None` style aliases and `X: TypeAlias = ...`
                    if let ExprKind::Name(n) = &targets[0].kind {
                        if looks_like_type_expr(value) {
                            if let Ok(t) = self.resolve_ann(value, m, &HashMap::new()) {
                                if let Some(Sym::Global(_)) = self.modules[m].scope.get(n) {
                                    self.modules[m].scope.insert(n.clone(), Sym::Alias(t));
                                }
                            }
                        }
                    }
                }
                StmtKind::AnnAssign { target, ann, value } => {
                    if let ExprKind::Name(n) = &target.kind {
                        if matches!(&ann.kind, ExprKind::Name(a) if a == "TypeAlias") {
                            if let Some(v) = value {
                                let t = self.resolve_ann(v, m, &HashMap::new())?;
                                self.modules[m].scope.insert(n.clone(), Sym::Alias(t));
                            }
                            continue;
                        }
                        let t = self.resolve_ann(ann, m, &HashMap::new())?;
                        if let Some(Sym::Global(g)) = self.modules[m].scope.get(n).cloned() {
                            self.globals[g].ty = Some(t);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn new_func(&mut self, m: usize, def: Rc<FuncDef>, owner: Option<ClassId>, kind: FnKind, subst: HashMap<String, Ty>, cname: String) -> usize {
        let mut extern_name = None;
        for d in &def.decorators {
            match &d.kind {
                ExprKind::Name(n) if n == "extern" => extern_name = Some(def.name.clone()),
                ExprKind::Call { func, args, .. } => {
                    if let ExprKind::Name(n) = &func.kind {
                        if n == "extern" {
                            if let Some(Expr { kind: ExprKind::Str { value, .. }, .. }) = args.first() {
                                extern_name = Some(value.clone());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let qualname = match owner {
            Some(c) => format!("{}.{}", self.classes[c].name, def.name),
            None => def.name.clone(),
        };
        let id = self.funcs.len();
        self.funcs.push(FuncInfo {
            name: def.name.clone(),
            qualname,
            module: m,
            def,
            owner,
            kind,
            subst,
            sig: None,
            extern_name,
            instances: HashMap::new(),
            scope: None,
            overrides: None,
            py_self: false,
            async_ret: None,
            py_class: None,
            cname_hint: None,
        });
        let _ = cname;
        id
    }

    pub fn new_class(&mut self, m: usize, def: Rc<ClassDef>, subst: HashMap<String, Ty>, targs: &[Ty], span: Span) -> ClassId {
        let modc = c_ident(&self.modules[m].name);
        let mut cname = format!("C_{}_{}", modc, c_ident(&def.name));
        if !targs.is_empty() {
            cname.push_str(&format!("_{}", self.classes.len()));
        }
        let mut name = def.name.clone();
        if !targs.is_empty() {
            name = format!("{}[{}]", name, targs.iter().map(|t| self.tyname(t)).collect::<Vec<_>>().join(", "));
        }
        let id = self.classes.len();
        self.classes.push(ClassInfo {
            name,
            cname,
            module: m,
            def,
            base: None,
            fields: Vec::new(),
            methods: Vec::new(),
            subst,
            is_dataclass: false,
            dc_order: false,
            dc_frozen: false,
            is_enum: false,
            enum_members: Vec::new(),
            subclasses: Vec::new(),
            resolved: false,
            class_vars: Vec::new(),
            span,
            is_protocol: false,
        });
        id
    }

    pub fn instantiate_template(&mut self, tid: usize, targs: Vec<Ty>, m: usize, span: Span) -> R<ClassId> {
        if let Some(&id) = self.templates[tid].instances.get(&targs) {
            return Ok(id);
        }
        let def = self.templates[tid].def.clone();
        if def.type_params.len() != targs.len() {
            return Err(self.err(m, span, format!("{} expects {} type arguments, got {}", def.name, def.type_params.len(), targs.len())));
        }
        let subst: HashMap<String, Ty> = def.type_params.iter().map(|p| p.name.clone()).zip(targs.iter().cloned()).collect();
        let tm = self.templates[tid].module;
        let id = self.new_class(tm, def, subst, &targs, span);
        self.templates[tid].instances.insert(targs, id);
        self.resolve_class(id)?;
        Ok(id)
    }

    /// Resolve bases, fields and methods of a class.
    pub fn resolve_class(&mut self, id: ClassId) -> R<()> {
        if self.classes[id].resolved {
            return Ok(());
        }
        self.classes[id].resolved = true;
        let def = self.classes[id].def.clone();
        let m = self.classes[id].module;
        let subst = self.classes[id].subst.clone();
        let modc = c_ident(&self.modules[m].name);
        // decorators
        for d in &def.decorators {
            let (name, kws) = match &d.kind {
                ExprKind::Name(n) => (n.clone(), vec![]),
                ExprKind::Attribute { attr, .. } => (attr.clone(), vec![]),
                ExprKind::Call { func, keywords, .. } => (
                    match &func.kind {
                        ExprKind::Name(n) => n.clone(),
                        ExprKind::Attribute { attr, .. } => attr.clone(),
                        _ => String::new(),
                    },
                    keywords.clone(),
                ),
                _ => (String::new(), vec![]),
            };
            if name == "dataclass" {
                self.classes[id].is_dataclass = true;
                for k in kws {
                    let on = matches!(k.value.kind, ExprKind::Bool(true));
                    match k.name.as_deref() {
                        Some("order") => self.classes[id].dc_order = on,
                        Some("frozen") => self.classes[id].dc_frozen = on,
                        _ => {}
                    }
                }
            }
        }
        // base class
        let mut base = None;
        for b in &def.bases {
            if let ExprKind::Name(n) = &b.kind {
                if n == "object" || n == "Generic" || n == "ABC" {
                    continue;
                }
                if n == "Protocol" {
                    self.classes[id].is_protocol = true;
                    continue;
                }
                if n == "Enum" || n == "IntEnum" {
                    self.classes[id].is_enum = true;
                    continue;
                }
            }
            if let ExprKind::Subscript { value, .. } = &b.kind {
                if matches!(&value.kind, ExprKind::Name(n) if n == "Generic" || n == "Protocol") {
                    continue;
                }
            }
            let t = self.resolve_ann(b, m, &subst)?;
            match t {
                Ty::Class(bid) => {
                    if base.is_some() {
                        return Err(self.err(m, b.span, "multiple inheritance of classes with state is not supported; use a single base class"));
                    }
                    base = Some(bid)
                }
                _ => return Err(self.err(m, b.span, format!("cannot inherit from {}", self.tyname(&t)))),
            }
        }
        if let Some(b) = base {
            self.resolve_class(b)?;
            self.classes[b].subclasses.push(id);
            self.classes[id].fields = self.classes[b].fields.clone();
            if self.classes[b].is_dataclass && !self.classes[id].is_dataclass {
                // subclass of a dataclass without decorator keeps base's generated methods
            }
        }
        self.classes[id].base = base;
        if self.classes[id].is_enum {
            let mut next = 1i64;
            for st in &def.body {
                if let StmtKind::Assign { targets, value } = &st.kind {
                    if let ExprKind::Name(n) = &targets[0].kind {
                        let v = match &value.kind {
                            ExprKind::Int(v) => *v,
                            ExprKind::UnaryOp { op: UnOp::Neg, operand } => match operand.kind {
                                ExprKind::Int(v) => -v,
                                _ => return Err(self.err(m, value.span, "enum values must be int literals or auto()")),
                            },
                            ExprKind::Call { .. } => next,
                            _ => return Err(self.err(m, value.span, "enum values must be int literals or auto()")),
                        };
                        next = v + 1;
                        self.classes[id].enum_members.push((n.clone(), v));
                    }
                }
            }
            self.classes[id].fields.push(Field { name: "name".into(), ty: Ty::Str, default: None, owner: id });
            self.classes[id].fields.push(Field { name: "value".into(), ty: Ty::Int, default: None, owner: id });
            let cn = self.classes[id].cname.clone();
            for (n, _) in self.classes[id].enum_members.clone() {
                let gid = self.globals.len();
                self.globals.push(GlobalVar { name: n.clone(), cname: format!("g_{}_{}", cn, c_ident(&n)), ty: Some(Ty::Class(id)), module: m, decl_span: Span::default(), py_import: None });
                self.classes[id].class_vars.push((n, gid));
            }
        }
        // fields from class-level annotations
        for st in &def.body {
            match &st.kind {
                StmtKind::AnnAssign { target, ann, value } => {
                    if let ExprKind::Name(n) = &target.kind {
                        if is_classvar(ann) {
                            let inner = classvar_inner(ann);
                            let t = match inner {
                                Some(i) => Some(self.resolve_ann(i, m, &subst)?),
                                None => None,
                            };
                            let gid = self.globals.len();
                            let cname = format!("g_{}_{}_{}", modc, c_ident(&self.classes[id].cname), c_ident(n));
                            self.globals.push(GlobalVar { name: n.clone(), cname, ty: t, module: m, decl_span: st.span, py_import: None });
                            self.classes[id].class_vars.push((n.clone(), gid));
                            continue;
                        }
                        let t = self.resolve_ann(ann, m, &subst)?;
                        if self.classes[id].fields.iter().any(|f| &f.name == n) {
                            continue;
                        }
                        // `name: T = v` that no method ever assigns through `self` is a class attribute
                        // `x: T = v` is a class variable when the program assigns it through the class
                        // (`C.x = ...`, `cls.x += ...`); otherwise an instance field with a default
                        let cls_name = def.name.clone();
                        let via_class = |b: &[Stmt]| assigns_attr(b, n, &|o: &str| o == cls_name || o == "cls");
                        let class_assigned = self.modules.iter().any(|md| via_class(&md.ast.body));
                        if value.is_some() && !self.classes[id].is_dataclass && class_assigned && !assigns_attr(&def.body, n, &|o: &str| o == "self") {
                            let gid = self.globals.len();
                            let cname = format!("g_{}_{}_{}", modc, c_ident(&self.classes[id].cname), c_ident(n));
                            self.globals.push(GlobalVar { name: n.clone(), cname, ty: Some(t), module: m, decl_span: st.span, py_import: None });
                            self.classes[id].class_vars.push((n.clone(), gid));
                            continue;
                        }
                        self.classes[id].fields.push(Field { name: n.clone(), ty: t, default: value.clone(), owner: id });
                    }
                }
                StmtKind::Assign { targets, .. } if !self.classes[id].is_enum => {
                    for t in targets {
                        if let ExprKind::Name(n) = &t.kind {
                            let gid = self.globals.len();
                            let cname = format!("g_{}_{}_{}", modc, c_ident(&self.classes[id].cname), c_ident(n));
                            self.globals.push(GlobalVar { name: n.clone(), cname, ty: None, module: m, decl_span: st.span, py_import: None });
                            self.classes[id].class_vars.push((n.clone(), gid));
                        }
                    }
                }
                _ => {}
            }
        }
        // synthesized dataclass methods
        let mut body: Vec<Stmt> = def.body.clone();
        if self.classes[id].is_dataclass {
            let synth = self.dataclass_methods(id)?;
            let parsed = parser::parse(&synth, &self.modules[m].path)?;
            for s in parsed.module.body {
                if let StmtKind::FuncDef(fd) = &s.kind {
                    if def.body.iter().any(|x| matches!(&x.kind, StmtKind::FuncDef(f) if f.name == fd.name)) {
                        continue;
                    }
                }
                body.push(s);
            }
        }
        // methods
        for st in &body {
            if let StmtKind::FuncDef(fd) = &st.kind {
                let mut kind = FnKind::Method;
                for d in &fd.decorators {
                    if let ExprKind::Name(n) = &d.kind {
                        match n.as_str() {
                            "staticmethod" => kind = FnKind::Static,
                            "classmethod" => kind = FnKind::ClassMethod,
                            "property" => kind = FnKind::Property,
                            _ => {}
                        }
                    }
                    if let ExprKind::Attribute { attr, .. } = &d.kind {
                        if attr == "setter" {
                            return Err(self.err(m, d.span, "property setters are not supported yet; use a method"));
                        }
                    }
                }
                let cname = format!("{}_{}", self.classes[id].cname, c_ident(&fd.name));
                let fid = self.new_func(m, Rc::new((**fd).clone()), Some(id), kind, subst.clone(), cname);
                self.classes[id].methods.push((fd.name.clone(), fid));
            }
        }
        // infer fields from `self.x = ...` / `self.x: T = ...` in __init__ when not declared
        if let Some(init) = self.own_method(id, "__init__") {
            let fd = self.funcs[init].def.clone();
            let mut param_tys: HashMap<String, Ty> = HashMap::new();
            for a in fd.params.all_positional().skip(1).chain(fd.params.kwonly.iter()) {
                if let Some(ann) = &a.ann {
                    param_tys.insert(a.name.clone(), self.resolve_ann(ann, m, &subst)?);
                }
            }
            if let Some(a) = &fd.params.vararg {
                if let Some(ann) = &a.ann {
                    let t = self.resolve_ann(ann, m, &subst)?;
                    param_tys.insert(a.name.clone(), Ty::List(Box::new(t)));
                }
            }
            if let Some(a) = &fd.params.kwarg {
                if let Some(ann) = &a.ann {
                    let t = self.resolve_ann(ann, m, &subst)?;
                    param_tys.insert(a.name.clone(), Ty::Dict(Box::new(Ty::Str), Box::new(t)));
                }
            }
            // element type when iterating a parameter's value
            let elem = |t: &Ty| -> Option<Ty> {
                match t {
                    Ty::List(e) | Ty::Set(e) | Ty::Iter(e) | Ty::Dict(e, _) => Some((**e).clone()),
                    Ty::Str => Some(Ty::Str),
                    Ty::Tuple(ts) if !ts.is_empty() && ts.iter().all(|x| *x == ts[0]) => Some(ts[0].clone()),
                    _ => None,
                }
            };
            let mut found: Vec<(String, Option<Ty>, Span, Option<Expr>)> = Vec::new();
            collect_self_assigns(&fd.body, &mut found);
            for (name, ann_ty, span, value) in found {
                if self.classes[id].fields.iter().any(|f| f.name == name) {
                    continue;
                }
                let ty = match ann_ty {
                    Some(t) => t,
                    None => {
                        // infer from simple RHS: parameter name or literal
                        match value.as_ref().map(|v| &v.kind) {
                            Some(ExprKind::Call { func, args, .. }) if matches!(&func.kind, ExprKind::Name(n) if n == "__ann__") => {
                                self.resolve_ann(&args[0], m, &subst)?
                            }
                            Some(ExprKind::Name(p)) if param_tys.contains_key(p) => param_tys[p].clone(),
                            // copies of a parameter: list(p), sorted(p), set(p), dict(p), p.copy()
                            Some(ExprKind::Call { func, args, .. })
                                if args.len() == 1
                                    && matches!(&func.kind, ExprKind::Name(f) if matches!(f.as_str(), "list" | "sorted" | "set" | "dict"))
                                    && matches!(&args[0].kind, ExprKind::Name(p) if param_tys.get(p).and_then(|t| elem(t)).is_some()) =>
                            {
                                let (ExprKind::Name(f), ExprKind::Name(p)) = (&func.kind, &args[0].kind) else { unreachable!() };
                                let pt = param_tys[p].clone();
                                let e = elem(&pt).unwrap();
                                match (f.as_str(), &pt) {
                                    ("set", _) => Ty::Set(Box::new(e)),
                                    ("dict", Ty::Dict(..)) => pt.clone(),
                                    _ => Ty::List(Box::new(e)),
                                }
                            }
                            Some(ExprKind::Call { func, args, .. })
                                if args.is_empty() && matches!(&func.kind, ExprKind::Attribute { value, attr } if attr == "copy" && matches!(&value.kind, ExprKind::Name(p) if matches!(param_tys.get(p), Some(Ty::List(_) | Ty::Dict(..) | Ty::Set(_))))) =>
                            {
                                let ExprKind::Attribute { value, .. } = &func.kind else { unreachable!() };
                                let ExprKind::Name(p) = &value.kind else { unreachable!() };
                                param_tys[p].clone()
                            }
                            Some(ExprKind::Int(_)) => Ty::Int,
                            Some(ExprKind::Float(_)) => Ty::Float,
                            Some(ExprKind::Bool(_)) => Ty::Bool,
                            Some(ExprKind::Str { .. }) | Some(ExprKind::FStr { .. }) => Ty::Str,
                            Some(ExprKind::UnaryOp { operand, .. }) if matches!(operand.kind, ExprKind::Int(_)) => Ty::Int,
                            Some(ExprKind::UnaryOp { operand, .. }) if matches!(operand.kind, ExprKind::Float(_)) => Ty::Float,
                            Some(ExprKind::Call { func, args, .. })
                                if matches!(&func.kind, ExprKind::Name(_)) && self.lookup_class_name(m, func, &subst).is_some() && !args.is_empty() || matches!(&func.kind, ExprKind::Name(n) if n == "str" || n == "int" || n == "float" || n == "len") =>
                            {
                                match &func.kind {
                                    ExprKind::Name(n) if n == "str" => Ty::Str,
                                    ExprKind::Name(n) if n == "int" || n == "len" => Ty::Int,
                                    ExprKind::Name(n) if n == "float" => Ty::Float,
                                    _ => Ty::Class(self.lookup_class_name(m, func, &subst).unwrap()),
                                }
                            }
                            Some(ExprKind::Call { func, .. }) if self.lookup_class_name(m, func, &subst).is_some() => {
                                Ty::Class(self.lookup_class_name(m, func, &subst).unwrap())
                            }
                            _ => {
                                return Err(self
                                    .err(m, span, format!("cannot infer the type of attribute '{}'", name))
                                    .note(format!("declare it in the class body, e.g. `{}: list[int]`, or annotate `self.{}: T = ...`", name, name)))
                            }
                        }
                    }
                };
                self.classes[id].fields.push(Field { name, ty, default: None, owner: id });
            }
        }
        Ok(())
    }

    fn lookup_class_name(&mut self, m: usize, e: &Expr, subst: &HashMap<String, Ty>) -> Option<ClassId> {
        match self.resolve_ann(e, m, subst) {
            Ok(Ty::Class(c)) => Some(c),
            _ => None,
        }
    }

    fn dataclass_methods(&mut self, id: ClassId) -> R<String> {
        let fields = self.classes[id].fields.clone();
        let cls = self.classes[id].def.name.clone();
        let mut s = String::new();
        // __init__
        let mut params = vec!["self".to_string()];
        let mut seen_default = false;
        for f in &fields {
            let ann = self.ann_src(&f.ty);
            if let Some(d) = &f.default {
                let dsrc = self.default_src(id, d)?;
                params.push(format!("{}: {} = {}", f.name, ann, dsrc));
                seen_default = true;
            } else {
                if seen_default {
                    return Err(self.err(self.classes[id].module, self.classes[id].span, format!("non-default field '{}' follows a default field", f.name)));
                }
                params.push(format!("{}: {}", f.name, ann));
            }
        }
        s.push_str(&format!("def __init__({}) -> None:\n", params.join(", ")));
        if fields.is_empty() {
            s.push_str("    pass\n");
        }
        for f in &fields {
            s.push_str(&format!("    self.{0} = {0}\n", f.name));
        }
        // __repr__
        let parts: Vec<String> = fields.iter().map(|f| format!("{0}={{self.{0}!r}}", f.name)).collect();
        s.push_str(&format!("def __repr__(self) -> str:\n    return f\"{}({})\"\n", cls, parts.join(", ")));
        // __eq__
        s.push_str(&format!("def __eq__(self, other: object) -> bool:\n    if not isinstance(other, {}):\n        return False\n", cls));
        if fields.is_empty() {
            s.push_str("    return True\n");
        } else {
            let cmp: Vec<String> = fields.iter().map(|f| format!("self.{0} == other.{0}", f.name)).collect();
            s.push_str(&format!("    return {}\n", cmp.join(" and ")));
        }
        if self.classes[id].dc_order {
            let a: Vec<String> = fields.iter().map(|f| format!("self.{}", f.name)).collect();
            let b: Vec<String> = fields.iter().map(|f| format!("other.{}", f.name)).collect();
            let (ta, tb) = if fields.len() == 1 { (a[0].clone(), b[0].clone()) } else { (format!("({},)", a.join(", ")), format!("({},)", b.join(", "))) };
            s.push_str(&format!("def __lt__(self, other: {}) -> bool:\n    return {} < {}\n", cls, ta, tb));
        }
        if self.classes[id].dc_frozen {
            let a: Vec<String> = fields.iter().map(|f| format!("self.{}", f.name)).collect();
            if !a.is_empty() {
                s.push_str(&format!("def __hash__(self) -> int:\n    return hash(({},))\n", a.join(", ")));
            }
        }
        Ok(s)
    }

    fn default_src(&self, id: ClassId, e: &Expr) -> R<String> {
        let m = self.classes[id].module;
        let src = &self.modules[m].src;
        let s = &src[e.span.start..e.span.end.min(src.len())];
        if s.is_empty() {
            return Err(self.err(m, e.span, "unsupported dataclass default"));
        }
        if s.contains("field(") {
            return Err(self.err(m, e.span, "dataclasses.field() is not supported yet; use an __init__"));
        }
        Ok(s.to_string())
    }

    /// Annotation source text for a type (used in synthesized code).
    pub fn ann_src(&self, t: &Ty) -> String {
        match t {
            Ty::Class(c) => {
                let ci = &self.classes[*c];
                if ci.subst.is_empty() {
                    ci.def.name.clone()
                } else {
                    let args: Vec<String> = ci.def.type_params.iter().map(|p| self.ann_src(&ci.subst[&p.name])).collect();
                    format!("{}[{}]", ci.def.name, args.join(", "))
                }
            }
            Ty::List(e) => format!("list[{}]", self.ann_src(e)),
            Ty::Set(e) => format!("set[{}]", self.ann_src(e)),
            Ty::Dict(k, v) => format!("dict[{}, {}]", self.ann_src(k), self.ann_src(v)),
            Ty::Tuple(ts) => format!("tuple[{}]", ts.iter().map(|t| self.ann_src(t)).collect::<Vec<_>>().join(", ")),
            Ty::Optional(i) => format!("{} | None", self.ann_src(i)),
            Ty::Union(ts) => ts.iter().map(|t| self.ann_src(t)).collect::<Vec<_>>().join(" | "),
            Ty::Iter(e) => format!("Iterator[{}]", self.ann_src(e)),
            Ty::Func(ps, r) => format!("Callable[[{}], {}]", ps.iter().map(|t| self.ann_src(t)).collect::<Vec<_>>().join(", "), self.ann_src(r)),
            Ty::Type(c) => format!("type[{}]", self.classes[*c].def.name),
            _ => self.tyname(t),
        }
    }

    pub fn own_method(&self, c: ClassId, name: &str) -> Option<usize> {
        self.classes[c].methods.iter().find(|(n, _)| n == name).map(|(_, f)| *f)
    }

    /// Find a method through the inheritance chain.
    pub fn find_method(&self, c: ClassId, name: &str) -> Option<usize> {
        let mut cur = Some(c);
        while let Some(id) = cur {
            if let Some(f) = self.own_method(id, name) {
                return Some(f);
            }
            cur = self.classes[id].base;
        }
        None
    }

    pub fn find_field(&self, c: ClassId, name: &str) -> Option<Field> {
        self.classes[c].fields.iter().find(|f| f.name == name).cloned()
    }

    pub fn find_class_var(&self, c: ClassId, name: &str) -> Option<usize> {
        let mut cur = Some(c);
        while let Some(id) = cur {
            if let Some((_, g)) = self.classes[id].class_vars.iter().find(|(n, _)| n == name) {
                return Some(*g);
            }
            cur = self.classes[id].base;
        }
        None
    }

    pub fn is_subclass(&self, a: ClassId, b: ClassId) -> bool {
        let mut cur = Some(a);
        while let Some(id) = cur {
            if id == b {
                return true;
            }
            cur = self.classes[id].base;
        }
        false
    }

    /// True if any subclass of `c` overrides method `name` (needs virtual dispatch).
    pub fn is_overridden(&self, c: ClassId, name: &str) -> bool {
        for &s in &self.classes[c].subclasses {
            if self.own_method(s, name).is_some() || self.is_overridden(s, name) {
                return true;
            }
        }
        false
    }

    // ------------------------------------------------------------------ annotations

    pub fn lookup_sym(&self, m: usize, name: &str) -> Option<Sym> {
        if let Some(s) = self.modules[m].scope.get(name) {
            return Some(s.clone());
        }
        if m != self.builtins_mod {
            if let Some(s) = self.modules[self.builtins_mod].scope.get(name) {
                if !name.starts_with('_') {
                    return Some(s.clone());
                }
            }
        }
        None
    }

    pub fn resolve_ann(&mut self, e: &Expr, m: usize, subst: &HashMap<String, Ty>) -> R<Ty> {
        match &e.kind {
            ExprKind::NoneLit => Ok(Ty::NoneT),
            ExprKind::Str { value, .. } => {
                let inner = parser::parse_expr_str(value, &self.modules[m].path, e.span)?;
                self.resolve_ann(&inner, m, subst)
            }
            ExprKind::Name(n) => {
                if let Some(t) = subst.get(n) {
                    return Ok(t.clone());
                }
                match self.lookup_sym(m, n) {
                    Some(Sym::Class(c)) => return Ok(Ty::Class(c)),
                    Some(Sym::Alias(t)) => return Ok(t),
                    // Python classes/objects used as annotations are dynamically typed values
                    Some(Sym::Global(g)) if self.globals[g].ty == Some(Ty::Any) && (self.globals[g].py_import.is_some() || self.py_classes.iter().any(|c| c.global == g)) => return Ok(Ty::Any),
                    Some(Sym::ClassTemplate(_)) => return Err(self.err(m, e.span, format!("generic class '{}' needs type arguments", n))),
                    Some(Sym::Builtin(b)) => return self.builtin_type_name(&b, m, e.span),
                    _ => {}
                }
                self.builtin_type_name(n, m, e.span)
            }
            ExprKind::Attribute { value, attr } => {
                if self.is_python_expr(m, value) {
                    return Ok(Ty::Any);
                }
                if let Some(mid) = self.module_of_expr(m, value) {
                    return match self.modules[mid].scope.get(attr).cloned() {
                        Some(Sym::Class(c)) => Ok(Ty::Class(c)),
                        Some(Sym::Alias(t)) => Ok(t),
                        _ => Err(self.err(m, e.span, format!("'{}.{}' is not a type", self.modules[mid].name, attr))),
                    };
                }
                if let ExprKind::Name(modname) = &value.kind {
                    match self.lookup_sym(m, modname) {
                        Some(Sym::Builtin(b)) if b.starts_with("module:") => return self.builtin_type_name(attr, m, e.span),
                        _ => {}
                    }
                }
                Err(self.err(m, e.span, "unsupported type expression"))
            }
            ExprKind::BinOp { left, op: BinOp::BitOr, right } => {
                let a = self.resolve_ann(left, m, subst)?;
                let b = self.resolve_ann(right, m, subst)?;
                Ok(make_union(vec![a, b]))
            }
            ExprKind::Subscript { value, index } => {
                if self.is_python_expr(m, value) {
                    return Ok(Ty::Any);
                }
                let args: Vec<Expr> = match &index.kind {
                    ExprKind::Tuple { elts, .. } => elts.clone(),
                    _ => vec![(**index).clone()],
                };
                let head = match &value.kind {
                    ExprKind::Name(n) => match self.lookup_sym(m, n) {
                        Some(Sym::ClassTemplate(tid)) => {
                            let mut targs = Vec::new();
                            for a in &args {
                                targs.push(self.resolve_ann(a, m, subst)?);
                            }
                            return Ok(Ty::Class(self.instantiate_template(tid, targs, m, e.span)?));
                        }
                        Some(Sym::Builtin(b)) => b,
                        _ => n.clone(),
                    },
                    ExprKind::Attribute { attr, value: v } => {
                        if let Some(mid) = self.module_of_expr(m, v) {
                            if let Some(Sym::ClassTemplate(tid)) = self.modules[mid].scope.get(attr).cloned() {
                                let mut targs = Vec::new();
                                for a in &args {
                                    targs.push(self.resolve_ann(a, m, subst)?);
                                }
                                return Ok(Ty::Class(self.instantiate_template(tid, targs, m, e.span)?));
                            }
                        }
                        attr.clone()
                    }
                    _ => return Err(self.err(m, e.span, "unsupported type expression")),
                };
                let ra = |c: &mut Compiler, i: usize| -> R<Ty> {
                    match args.get(i) {
                        Some(a) => c.resolve_ann(a, m, subst),
                        None => Err(c.err(m, e.span, format!("{} needs more type arguments", head))),
                    }
                };
                match head.as_str() {
                    "list" | "List" | "Sequence" | "MutableSequence" | "deque" => Ok(Ty::list(ra(self, 0)?)),
                    "set" | "Set" | "frozenset" | "FrozenSet" | "AbstractSet" | "MutableSet" => Ok(Ty::set(ra(self, 0)?)),
                    "dict" | "Dict" | "Mapping" | "MutableMapping" | "defaultdict" | "OrderedDict" => Ok(Ty::dict(ra(self, 0)?, ra(self, 1)?)),
                    "tuple" | "Tuple" => {
                        if args.len() == 2 && matches!(args[1].kind, ExprKind::Ellipsis) {
                            return Ok(Ty::VarTuple(Box::new(ra(self, 0)?)));
                        }
                        if args.len() == 1 {
                            if let ExprKind::Tuple { elts, .. } = &args[0].kind {
                                if elts.is_empty() {
                                    return Ok(Ty::Tuple(vec![]));
                                }
                            }
                        }
                        let mut ts = Vec::new();
                        for i in 0..args.len() {
                            ts.push(ra(self, i)?);
                        }
                        Ok(Ty::Tuple(ts))
                    }
                    "Optional" => Ok(Ty::optional(ra(self, 0)?)),
                    "Union" => {
                        let mut ts = Vec::new();
                        for i in 0..args.len() {
                            ts.push(ra(self, i)?);
                        }
                        Ok(make_union(ts))
                    }
                    "Iterator" | "Iterable" | "Generator" | "AsyncIterator" | "Reversible" | "Collection" | "Container" => {
                        Ok(Ty::iter(ra(self, 0)?))
                    }
                    "Callable" => {
                        let params = match args.first().map(|a| &a.kind) {
                            Some(ExprKind::List(ps)) => {
                                let mut out = Vec::new();
                                for p in ps {
                                    out.push(self.resolve_ann(p, m, subst)?);
                                }
                                out
                            }
                            _ => return Err(self.err(m, e.span, "Callable[...] requires an explicit parameter list, e.g. Callable[[int], str]")),
                        };
                        Ok(Ty::Func(params, Box::new(ra(self, 1)?)))
                    }
                    "type" | "Type" => match ra(self, 0)? {
                        Ty::Class(c) => Ok(Ty::Type(c)),
                        t => Err(self.err(m, e.span, format!("type[{}] is not supported", self.tyname(&t)))),
                    },
                    "Final" | "ClassVar" | "Annotated" | "Required" | "NotRequired" | "ReadOnly" => ra(self, 0),
                    "Literal" => {
                        let lit = &args[0];
                        Ok(match &lit.kind {
                            ExprKind::Int(_) => Ty::Int,
                            ExprKind::Str { .. } => Ty::Str,
                            ExprKind::Bool(_) => Ty::Bool,
                            ExprKind::Float(_) => Ty::Float,
                            _ => return Err(self.err(m, e.span, "unsupported Literal type")),
                        })
                    }
                    _ => Err(self.err(m, e.span, format!("unknown generic type '{}'", head))),
                }
            }
            ExprKind::Ellipsis => Err(self.err(m, e.span, "'...' is not a type here")),
            _ => Err(self.err(m, e.span, "invalid type annotation")),
        }
    }

    fn builtin_type_name(&mut self, n: &str, m: usize, span: Span) -> R<Ty> {
        Ok(match n {
            "int" => Ty::Int,
            "float" => Ty::Float,
            "bool" => Ty::Bool,
            "str" => Ty::Str,
            "bytes" | "bytearray" => Ty::Bytes,
            "None" | "NoneType" => Ty::NoneT,
            "object" | "Any" => Ty::Any,
            "Never" | "NoReturn" => Ty::Never,
            "Self" => {
                if let Some(fx) = self.fx_stack.last() {
                    if let Some(c) = fx.class {
                        return Ok(Ty::Class(c));
                    }
                }
                return Err(self.err(m, span, "Self is only valid inside a class"));
            }
            "list" | "dict" | "set" | "tuple" | "List" | "Dict" | "Set" | "Tuple" => {
                return Err(self.err(m, span, format!("'{}' needs type arguments, e.g. {}[int]", n, n.to_lowercase())))
            }
            _ => return Err(self.err(m, span, format!("unknown type '{}'", n))),
        })
    }

    // ------------------------------------------------------------------ signatures

    pub fn func_sig(&mut self, fid: usize) -> R<Sig> {
        if let Some(s) = &self.funcs[fid].sig {
            return Ok(s.clone());
        }
        let f = &self.funcs[fid];
        let def = f.def.clone();
        let m = f.module;
        let mut subst = f.subst.clone();
        for tp in &def.type_params {
            subst.insert(tp.name.clone(), Ty::Var(tp.name.clone()));
        }
        let owner = f.owner;
        let kind = f.kind;
        let py_self = f.py_self;
        let saved = self.fx_stack.len();
        // Allow `Self` resolution while resolving method signatures.
        if owner.is_some() {
            self.fx_stack.push(func::Fx::new(m, owner));
        }
        let res = (|| -> R<Sig> {
            let mut params = Vec::new();
            let all: Vec<&Arg> = def.params.all_positional().collect();
            for (i, a) in all.iter().enumerate() {
                let is_self = i == 0 && matches!(kind, FnKind::Method | FnKind::Property | FnKind::ClassMethod);
                let ty = if i == 0 && py_self {
                    Ty::Any
                } else if is_self {
                    match kind {
                        FnKind::ClassMethod => Ty::Type(owner.unwrap()),
                        _ => Ty::Class(owner.unwrap()),
                    }
                } else {
                    match &a.ann {
                        Some(ann) => self.resolve_ann(ann, m, &subst)?,
                        None => {
                            return Err(self
                                .err(m, a.span, format!("parameter '{}' of '{}' needs a type annotation", a.name, def.name))
                                .note("fpython is strictly typed; write `name: Any` to opt into dynamic typing"))
                        }
                    }
                };
                params.push(Param { name: a.name.clone(), ty, default: a.default.clone(), kwonly: false });
            }
            for a in &def.params.kwonly {
                let ty = match &a.ann {
                    Some(ann) => self.resolve_ann(ann, m, &subst)?,
                    None => return Err(self.err(m, a.span, format!("parameter '{}' needs a type annotation", a.name))),
                };
                params.push(Param { name: a.name.clone(), ty, default: a.default.clone(), kwonly: true });
            }
            let vararg = match &def.params.vararg {
                Some(a) => {
                    let t = match &a.ann {
                        Some(ann) => self.resolve_ann(ann, m, &subst)?,
                        None => return Err(self.err(m, a.span, format!("*{} needs a type annotation", a.name))),
                    };
                    Some((a.name.clone(), Ty::list(t)))
                }
                None => None,
            };
            let kwarg = match &def.params.kwarg {
                Some(a) => {
                    let t = match &a.ann {
                        Some(ann) => self.resolve_ann(ann, m, &subst)?,
                        None => return Err(self.err(m, a.span, format!("**{} needs a type annotation", a.name))),
                    };
                    Some((a.name.clone(), Ty::dict(Ty::Str, t)))
                }
                None => None,
            };
            let ret = match &def.returns {
                Some(r) => self.resolve_ann(r, m, &subst)?,
                None => {
                    if def.name == "__init__" {
                        Ty::NoneT
                    } else {
                        return Err(self
                            .err(m, Span { line: def.body.first().map(|s| s.span.line.saturating_sub(1)).unwrap_or(0).max(1), col: 1, start: 0, end: 0 }, format!("function '{}' needs a return type annotation", def.name))
                            .note("use `-> None` for functions that return nothing"));
                    }
                }
            };
            Ok(Sig { params, vararg, kwarg, ret })
        })();
        self.fx_stack.truncate(saved);
        let mut sig = res?;
        if def.is_async {
            // `async def f() -> T`: calling it yields an awaitable (a Python coroutine object)
            self.uses_python = true;
            self.funcs[fid].async_ret = Some(sig.ret.clone());
            sig.ret = Ty::Any;
            self.funcs[fid].sig = Some(sig.clone());
            return Ok(sig);
        }
        // generator functions: return type Iterator[T]
        if def.type_params.is_empty() {
            if let Ty::Iter(_) = sig.ret {
            } else if self.func_scope(fid).has_yield {
                return Err(self.err(m, def.body[0].span, format!("generator '{}' must be annotated to return Iterator[T]", def.name)));
            }
        }
        if let Some(_) = &self.funcs[fid].extern_name {
            for p in &mut sig.params {
                p.default = p.default.take();
            }
        }
        self.funcs[fid].sig = Some(sig.clone());
        Ok(sig)
    }

    pub fn func_scope(&mut self, fid: usize) -> Rc<Scope> {
        if let Some(s) = &self.funcs[fid].scope {
            return s.clone();
        }
        let def = self.funcs[fid].def.clone();
        let s = Rc::new(scope::analyze_function(&def.params, &def.body));
        self.funcs[fid].scope = Some(s.clone());
        s
    }

    // ------------------------------------------------------------------ type relations

    pub fn assignable(&self, from: &Ty, to: &Ty) -> bool {
        if from == to {
            return true;
        }
        match (from, to) {
            (_, Ty::Any) | (Ty::Any, _) | (Ty::Never, _) => true,
            (Ty::Int, Ty::Float) | (Ty::Bool, Ty::Int) | (Ty::Bool, Ty::Float) => true,
            (Ty::NoneT, Ty::Optional(_)) => true,
            (Ty::Optional(a), Ty::Optional(b)) => {
                let exact_member = |t: &Ty, ms: &[Ty]| ms.contains(t) || matches!(t, Ty::Union(ts) if ts.iter().all(|x| ms.contains(x)));
                a == b
                    || (!a.is_scalar() && !b.is_scalar() && self.assignable(a, b) && !a.is_boxed() && !b.is_boxed())
                    // both boxed: `int | None` into `int | str | None` keeps the same object
                    || matches!(&**b, Ty::Union(ms) if exact_member(a, ms))
            }
            (f, Ty::Optional(b)) => {
                if b.is_scalar() {
                    matches!((f, &**b), (Ty::Int, Ty::Int) | (Ty::Float, Ty::Float) | (Ty::Bool, Ty::Bool) | (Ty::Int, Ty::Float) | (Ty::Bool, Ty::Int))
                } else {
                    self.assignable(f, b)
                }
            }
            (Ty::Class(a), Ty::Class(b)) => self.is_subclass(*a, *b) || self.implements_protocol(*a, *b),
            (Ty::Type(a), Ty::Type(b)) => self.is_subclass(*a, *b),
            (f, Ty::Union(ms)) => match f {
                Ty::Union(fs) => fs.iter().all(|x| ms.iter().any(|m| self.assignable(x, m))),
                _ => ms.iter().any(|m| self.assignable(f, m)),
            },
            (Ty::List(a), Ty::Iter(b)) | (Ty::Set(a), Ty::Iter(b)) | (Ty::Iter(a), Ty::Iter(b)) => a == b,
            (Ty::Dict(k, _), Ty::Iter(b)) => **k == **b,
            (Ty::Str, Ty::Iter(b)) => **b == Ty::Str,
            (Ty::Bytes, Ty::Iter(b)) => **b == Ty::Int,
            (Ty::Tuple(ts), Ty::Iter(b)) => ts.iter().all(|t| t == &**b),
            // a fixed tuple is a tuple[T, ...] when every item converts to T (coerce rebuilds it)
            (Ty::Tuple(ts), Ty::VarTuple(b)) => ts.iter().all(|t| t == &**b || self.assignable(t, b)),
            (Ty::VarTuple(a), Ty::VarTuple(b)) => **b == Ty::Any || a == b,
            (Ty::VarTuple(a), Ty::Iter(b)) => a == b,
            (Ty::Tuple(a), Ty::Tuple(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x == y || (y == &Ty::Any) || (x.is_scalar() == y.is_scalar() && x.kind() == y.kind() && self.assignable(x, y) && !y.is_boxed() && !x.is_boxed())),
            (Ty::Func(pa, ra), Ty::Func(pb, rb)) => pa.len() == pb.len() && pa.iter().zip(pb).all(|(a, b)| a == b) && (ra == rb || **rb == Ty::NoneT),
            (Ty::Class(c), Ty::Iter(e)) => {
                // classes with __iter__ returning Iterator[e]
                if let Some(f) = self.find_method(*c, "__iter__") {
                    if let Some(sig) = &self.funcs[f].sig {
                        return sig.ret == Ty::Iter(e.clone());
                    }
                }
                false
            }
            _ => false,
        }
    }

    fn implements_protocol(&self, a: ClassId, p: ClassId) -> bool {
        if !self.classes[p].is_protocol {
            return false;
        }
        self.classes[p].methods.iter().all(|(n, _)| self.find_method(a, n).is_some())
    }

    /// Coerce a C expression of type `from` to type `to`. Caller ensures assignability.
    pub fn coerce(&mut self, c: &str, from: &Ty, to: &Ty) -> String {
        if from == to {
            return c.to_string();
        }
        match (from, to) {
            (Ty::Never, t) => format!("(({})({}))", self.ctype(t), c),
            (Ty::Int, Ty::Float) | (Ty::Bool, Ty::Float) => format!("((double)({}))", c),
            (Ty::Bool, Ty::Int) => format!("((fpy_int)({}))", c),
            (Ty::NoneT, t) => format!("(({})NULL)", self.ctype(t)),
            (f, t) if t.is_boxed() && !f.is_boxed() => {
                // box into FpyObj*
                let inner = match t {
                    Ty::Optional(i) if i.is_scalar() => i.as_ref().clone(),
                    _ => f.clone(),
                };
                let src = if f.is_scalar() && inner.is_scalar() && f != &inner { self.coerce(c, f, &inner) } else { c.to_string() };
                match if f.is_scalar() && inner.is_scalar() { &inner } else { f } {
                    Ty::Int => format!("fpy_box_int({})", src),
                    Ty::Float => format!("fpy_box_float({})", src),
                    Ty::Bool => format!("fpy_box_bool({})", src),
                    Ty::Optional(x) if !x.is_scalar() => format!("((FpyObj*)({}))", src),
                    _ => format!("((FpyObj*)({}))", src),
                }
            }
            (f, t) if f.is_boxed() && t.is_boxed() => c.to_string(),
            (f, t) if f.is_boxed() => self.unbox(c, t),
            (Ty::Class(_), Ty::Class(_)) | (Ty::Class(_), Ty::Optional(_)) | (Ty::Optional(_), Ty::Optional(_)) => {
                format!("(({})({}))", self.ctype(to), c)
            }
            (Ty::List(_), Ty::Iter(_)) => format!("fpy_list_iter({})", c),
            (Ty::Set(_), Ty::Iter(_)) => format!("fpy_set_iter({})", c),
            (Ty::Dict(..), Ty::Iter(_)) => format!("fpy_dict_iter_keys({})", c),
            (Ty::Str, Ty::Iter(_)) => format!("fpy_str_iter({})", c),
            (Ty::Bytes, Ty::Iter(_)) => format!("fpy_bytes_iter({})", c),
            (Ty::Tuple(_), Ty::Iter(_)) | (Ty::VarTuple(_), Ty::Iter(_)) => format!("fpy_tuple_iter({})", c),
            (Ty::Tuple(ts), Ty::VarTuple(b)) => {
                if ts.iter().all(|t| t == &**b) && !ts.is_empty() {
                    c.to_string()
                } else {
                    // rebuild with every item converted to the element type
                    let ect = self.ctype(b);
                    let mut code = format!("({{ FpyTuple *_s = {}; FpyTuple *_t = fpy_tuple_uniform({}, {}); ", c, ts.len(), b.kind());
                    for (i, t) in ts.iter().enumerate() {
                        let tct = self.ctype(t);
                        let item = t.from_word(&format!("_s->items[{}]", i), &tct);
                        let conv = self.coerce(&item, t, b);
                        let _ = &ect;
                        code.push_str(&format!("_t->items[{}] = {}; ", i, b.to_word(&conv)));
                    }
                    code.push_str("_t; })");
                    code
                }
            }
            (Ty::VarTuple(_), Ty::VarTuple(b)) if **b == Ty::Any => format!("fpy_tuple_retype({}, FK_OBJ)", c),
            (Ty::Class(cid), Ty::Iter(et)) => {
                let f = self.find_method(*cid, "__iter__").unwrap();
                let cname = self.method_cname(f);
                let owner = self.classes[self.funcs[f].owner.unwrap()].cname.clone();
                let call = format!("{}((struct {}*)({}))", cname, owner, c);
                match self.funcs[f].sig.as_ref().map(|s| s.ret.clone()) {
                    // __iter__ returns an object implementing __next__: adapt the protocol
                    Some(Ty::Class(_)) => format!("fpy_iter_from_protocol((FpyObj*){}, {})", call, et.kind()),
                    _ => call,
                }
            }
            (Ty::Tuple(_), Ty::Tuple(_)) => c.to_string(),
            (Ty::Func(..), Ty::Func(..)) => c.to_string(),
            (Ty::Type(_), Ty::Type(_)) => c.to_string(),
            _ => c.to_string(),
        }
    }

    /// Unbox an `FpyObj*` (Any/union/optional-scalar) into a concrete type with a runtime check.
    pub fn unbox(&mut self, c: &str, to: &Ty) -> String {
        match to {
            Ty::Int => format!("((fpy_int)fpy_unbox_word({}, FK_INT, \"int\"))", c),
            Ty::Float => format!("fpy_fw(fpy_unbox_word({}, FK_FLOAT, \"float\"))", c),
            Ty::Bool => format!("(fpy_unbox_word({}, FK_BOOL, \"bool\") != 0)", c),
            Ty::Str => format!("((FpyStr*)fpy_check_cast({}, &fpy_type_str, \"str\"))", c),
            Ty::Bytes => format!("((FpyBytes*)fpy_check_cast({}, &fpy_type_bytes, \"bytes\"))", c),
            Ty::List(_) => format!("((FpyList*)fpy_check_cast({}, &fpy_type_list, \"list\"))", c),
            Ty::Dict(..) => format!("((FpyDict*)fpy_check_cast({}, &fpy_type_dict, \"dict\"))", c),
            Ty::Set(_) => format!("((FpyDict*)fpy_check_cast({}, &fpy_type_set, \"set\"))", c),
            Ty::Tuple(_) => format!("((FpyTuple*)fpy_check_cast({}, &fpy_type_tuple, \"tuple\"))", c),
            Ty::VarTuple(e) => format!("fpy_tuple_retype((FpyTuple*)fpy_check_cast({}, &fpy_type_tuple, \"tuple\"), {})", c, e.kind()),
            Ty::Class(id) => {
                let cn = self.class_cname(*id);
                let name = self.classes[*id].name.clone();
                format!("((struct {}*)fpy_check_cast({}, &T_{}, {}))", cn, c, cn, c_string(name.as_bytes()))
            }
            Ty::Optional(inner) if !inner.is_scalar() => {
                let t = self.fresh("o");
                let ct = self.ctype(to);
                let un = self.unbox(&t, inner);
                format!("({{ FpyObj* {t} = (FpyObj*)({c}); {t} ? ({ct}){un} : ({ct})NULL; }})")
            }
            Ty::Func(ps, r) => {
                // a callable from dynamic code has no typed entry for this signature: wrap it
                let ad = self.func_adapter(ps, r);
                format!("({{ FpyObj *_f = (FpyObj*)({c}); _f ? fpy_func_new((void*){ad}, _f, {ad}_dyn, \"<callable>\") : NULL; }})")
            }
            Ty::Iter(e) => format!("fpy_dyn_iter_kind({}, {})", c, e.kind()),
            Ty::NoneT => format!("((FpyObj*)({}))", c),
            _ => c.to_string(),
        }
    }

    /// C expression for the prelude's `_NO_ARG` sentinel (exception constructed without arguments).
    pub fn exc_no_arg(&self) -> String {
        match self.modules[self.builtins_mod].scope.get("_NO_ARG") {
            Some(Sym::Global(g)) => format!("G.{}", self.globals[*g].cname),
            _ => "NULL".into(),
        }
    }

    /// A typed entry `R fn(void *env, P...)` that calls the dynamic callable `env` (boxing the
    /// arguments, unboxing the result), plus its dynamic entry `<name>_dyn`.
    fn func_adapter(&mut self, ps: &[Ty], r: &Ty) -> String {
        let key = format!("{:?}->{:?}", ps, r);
        if let Some(n) = self.func_adapters.get(&key) {
            return n.clone();
        }
        let name = format!("FA_{}", self.func_adapters.len());
        self.func_adapters.insert(key, name.clone());
        let params: Vec<String> = ps.iter().enumerate().map(|(i, p)| format!("{} a{}", self.ctype(p), i)).collect();
        let boxed: Vec<String> = ps.iter().enumerate().map(|(i, p)| self.coerce(&format!("a{}", i), p, &Ty::Any)).collect();
        let rct = self.ctype(r);
        let call = format!("fpy_dyn_call((FpyObj*)env, (FpyObj*[]){{{}}}, {})", if boxed.is_empty() { "NULL".into() } else { boxed.join(", ") }, ps.len());
        let body = if *r == Ty::NoneT { format!("{}; return NULL;", call) } else { format!("FpyObj *_r = {}; return {};", call, self.unbox("_r", r)) };
        let mut sig = vec!["void *env".to_string()];
        sig.extend(params);
        self.out.protos.push_str(&format!("static {} {}({});
static FpyObj *{}_dyn(void *env, FpyObj **a, int64_t n);
", rct, name, sig.join(", "), name));
        self.out.funcs.push_str(&format!(
            "static {} {}({}) {{\n    {}\n}}\nstatic FpyObj *{}_dyn(void *env, FpyObj **a, int64_t n) {{ return fpy_dyn_call((FpyObj*)env, a, n); }}\n\n",
            rct,
            name,
            sig.join(", "),
            body,
            name
        ));
        name
    }

    pub fn method_cname(&mut self, fid: usize) -> String {
        match self.func_instance(fid, vec![]) {
            Ok(n) => n,
            Err(_) => format!("{}_{}", self.classes[self.funcs[fid].owner.unwrap()].cname, c_ident(&self.funcs[fid].name)),
        }
    }

    /// Get (or schedule) the C function for a function instance.
    pub fn func_instance(&mut self, fid: usize, targs: Vec<Ty>) -> R<String> {
        if let Some(n) = self.funcs[fid].instances.get(&targs) {
            return Ok(n.clone());
        }
        if let Some(ext) = &self.funcs[fid].extern_name {
            return Ok(ext.clone());
        }
        let f = &self.funcs[fid];
        let base = match (&f.cname_hint, f.owner) {
            (Some(h), _) => h.clone(),
            (None, Some(c)) => format!("{}_{}", self.classes[c].cname, c_ident(&f.name)),
            (None, None) => format!("f_{}_{}", c_ident(&self.modules[f.module].name), c_ident(&f.name)),
        };
        let cname = if targs.is_empty() { base } else { format!("{}__{}", base, self.funcs[fid].instances.len()) };
        self.funcs[fid].instances.insert(targs.clone(), cname.clone());
        self.queue.push_back(Job { func: fid, targs, cname: cname.clone() });
        Ok(cname)
    }

    pub fn string_lit(&mut self, s: &str) -> String {
        if let Some(n) = self.strings.get(s) {
            return format!("((FpyStr*)&{})", n);
        }
        let name = format!("S{}", self.strings.len());
        let bytes = s.as_bytes();
        let nchars = s.chars().count();
        self.out.strings.push_str(&format!(
            "static struct {{ const FpyType *ty; int64_t len, nchars, hash; char data[{}]; }} {} = {{&fpy_type_str, {}, {}, 0, {}}};\n",
            bytes.len() + 1,
            name,
            bytes.len(),
            nchars,
            c_string(bytes)
        ));
        self.strings.insert(s.to_string(), name.clone());
        format!("((FpyStr*)&{})", name)
    }

    pub fn kinds_array(&mut self, kinds: &[u8]) -> String {
        if let Some(n) = self.tuple_kinds.get(kinds) {
            return n.clone();
        }
        let name = format!("K{}", self.tuple_kinds.len());
        self.out.strings.push_str(&format!(
            "static const uint8_t {}[{}] = {{{}}};\n",
            name,
            kinds.len().max(1),
            kinds.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(", ")
        ));
        self.tuple_kinds.insert(kinds.to_vec(), name.clone());
        name
    }

    // ------------------------------------------------------------------ program

    pub fn compile_program(&mut self, entry: &Path) -> R<String> {
        let src = std::fs::read_to_string(entry).map_err(|e| Diag::new(&entry.to_string_lossy(), Span::default(), format!("cannot read: {}", e)))?;
        let bsrc = STDLIB[0].1.to_string();
        self.builtins_mod = self.load_module_src("builtins", "<stdlib>/builtins.fpy", bsrc)?;
        self.declare_module(self.builtins_mod)?;
        let main = self.load_module_src("__main__", &entry.to_string_lossy(), src)?;
        self.declare_module(main)?;
        // Resolve all classes (including those in imported modules).
        let mut i = 0;
        while i < self.classes.len() {
            self.resolve_class(i)?;
            i += 1;
        }
        // iteration protocol signatures are needed by assignable() (Class -> Iterable[T])
        // regardless of the order in which code is compiled
        for c in 0..self.classes.len() {
            for name in ["__iter__", "__next__"] {
                if let Some(f) = self.own_method(c, name) {
                    if self.classes[c].subst.values().all(|t| !t.has_var()) {
                        let _ = self.func_sig(f);
                    }
                }
            }
        }
        // Infer types of module-level globals, in module dependency order.
        for m in 0..self.modules.len() {
            self.infer_module_globals(m)?;
        }
        // Compile all functions reachable from module inits; also all methods of all classes.
        for m in 0..self.modules.len() {
            self.compile_module_init(m)?;
        }
        let mut c = 0;
        while c < self.classes.len() || !self.queue.is_empty() {
            while let Some(job) = self.queue.pop_front() {
                self.compile_function(job)?;
            }
            if c < self.classes.len() {
                self.emit_class(c)?;
                c += 1;
            }
        }
        Ok(self.assemble(main))
    }

    fn infer_module_globals(&mut self, m: usize) -> R<()> {
        if self.modules[m].inferred {
            return Ok(());
        }
        self.modules[m].inferred = true;
        // modules this one imports (directly or through imported names) go first
        let mut deps: Vec<usize> = Vec::new();
        for sym in self.modules[m].scope.values() {
            match sym {
                Sym::Module(id) => deps.push(*id),
                Sym::Global(g) => deps.push(self.globals[*g].module),
                _ => {}
            }
        }
        deps.sort();
        deps.dedup();
        for d in deps {
            if d != m {
                self.infer_module_globals(d)?;
            }
        }
        let ast = self.modules[m].ast.clone();
        // Type-only pass over module statements, to fix global types before any function body is compiled.
        let mut fx = func::Fx::new(m, None);
        fx.is_module = true;
        fx.dry_run = true;
        self.fx_stack.push(fx);
        let r = (|| -> R<()> {
            for st in &ast.body {
                self.infer_global_stmt(st)?;
            }
            Ok(())
        })();
        self.fx_stack.pop();
        r
    }

    /// The native module a (dotted) name expression refers to: `m`, `pkg.mod`, `alias.sub`.
    fn module_of_expr(&self, m: usize, e: &Expr) -> Option<usize> {
        match &e.kind {
            ExprKind::Name(n) => match self.lookup_sym(m, n) {
                Some(Sym::Module(id)) => Some(id),
                _ => None,
            },
            ExprKind::Attribute { value, attr } => {
                let parent = self.module_of_expr(m, value)?;
                match self.modules[parent].scope.get(attr) {
                    Some(Sym::Module(id)) => Some(*id),
                    _ => self.mod_index.get(&format!("{}.{}", self.modules[parent].name, attr)).copied(),
                }
            }
            _ => None,
        }
    }

    /// A module-level assignment that the declaration pass turned into a type alias.
    pub fn is_alias_stmt(&self, st: &Stmt) -> bool {
        let Some(fx) = self.fx_stack.last() else { return false };
        if !fx.is_module {
            return false;
        }
        let target = match &st.kind {
            StmtKind::Assign { targets, .. } if targets.len() == 1 => &targets[0],
            StmtKind::AnnAssign { target, .. } => target,
            _ => return false,
        };
        matches!(&target.kind, ExprKind::Name(n) if matches!(self.modules[fx.module].scope.get(n), Some(Sym::Alias(_))))
    }

    fn infer_global_stmt(&mut self, st: &Stmt) -> R<()> {
        if self.is_alias_stmt(st) {
            return Ok(());
        }
        let m = self.fx_stack.last().unwrap().module;
        match &st.kind {
            StmtKind::Assign { targets, value } => {
                let names: Vec<&Expr> = targets.iter().collect();
                let mut need = false;
                for t in &names {
                    if let ExprKind::Name(n) = &t.kind {
                        if let Some(Sym::Global(g)) = self.modules[m].scope.get(n) {
                            if self.globals[*g].ty.is_none() {
                                need = true;
                            }
                        }
                    } else if let ExprKind::Tuple { .. } = &t.kind {
                        need = true;
                    }
                }
                if !need {
                    return Ok(());
                }
                let ce = self.expr(value, None)?;
                for t in names {
                    self.bind_global_types(t, &ce.ty, m)?;
                }
            }
            StmtKind::For { target, iter, body, orelse, .. } => {
                let ce = self.expr(iter, None)?;
                if let Some(et) = self.elem_type(&ce.ty) {
                    self.bind_global_types(target, &et, m)?;
                }
                for s in body.iter().chain(orelse) {
                    self.infer_global_stmt(s)?;
                }
            }
            StmtKind::If { body, orelse, .. } | StmtKind::While { body, orelse, .. } => {
                for s in body.iter().chain(orelse) {
                    self.infer_global_stmt(s)?;
                }
            }
            StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
                for s in body.iter().chain(orelse).chain(finalbody) {
                    self.infer_global_stmt(s)?;
                }
                for h in handlers {
                    if let Some(n) = &h.name {
                        let t = match &h.typ {
                            Some(e) => self.resolve_exc_type(e, m)?,
                            None => self.exc_base_ty(),
                        };
                        if let Some(Sym::Global(g)) = self.modules[m].scope.get(n).cloned() {
                            if self.globals[g].ty.is_none() {
                                self.globals[g].ty = Some(t);
                            }
                        }
                    }
                    for s in &h.body {
                        self.infer_global_stmt(s)?;
                    }
                }
            }
            StmtKind::With { items, body, .. } => {
                for it in items {
                    if let Some(v) = &it.var {
                        let ce = self.expr(&it.ctx, None)?;
                        let et = self.enter_type(&ce.ty, it.ctx.span)?;
                        self.bind_global_types(v, &et, m)?;
                    }
                }
                for s in body {
                    self.infer_global_stmt(s)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn bind_global_types(&mut self, t: &Expr, ty: &Ty, m: usize) -> R<()> {
        match &t.kind {
            ExprKind::Name(n) => {
                if let Some(Sym::Global(g)) = self.modules[m].scope.get(n).cloned() {
                    if self.globals[g].ty.is_none() {
                        let ty = self.infer_var_type(ty, t.span, n)?;
                        self.globals[g].ty = Some(ty);
                    }
                }
            }
            ExprKind::Tuple { elts, .. } | ExprKind::List(elts) => {
                let ets: Vec<Ty> = match ty {
                    Ty::Tuple(ts) if ts.len() == elts.len() => ts.clone(),
                    Ty::List(e) => vec![(**e).clone(); elts.len()],
                    _ => return Ok(()),
                };
                for (e, et) in elts.iter().zip(ets) {
                    self.bind_global_types(e, &et, m)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Validate an inferred variable type (reject un-inferable empty containers etc.).
    pub fn infer_var_type(&self, ty: &Ty, span: Span, name: &str) -> R<Ty> {
        let m = self.fx_stack.last().map(|f| f.module).unwrap_or(0);
        if has_unknown(ty) {
            return Err(self
                .err(m, span, format!("cannot infer the type of '{}' from an empty literal", name))
                .note(format!("annotate it, e.g. `{}: list[int] = []`", name)));
        }
        if *ty == Ty::NoneT {
            return Err(self.err(m, span, format!("cannot infer the type of '{}' from None", name)).note(format!("annotate it, e.g. `{}: Foo | None = None`", name)));
        }
        Ok(ty.clone())
    }

    pub fn exc_base_ty(&self) -> Ty {
        match self.modules[self.builtins_mod].scope.get("BaseException") {
            Some(Sym::Class(c)) => Ty::Class(*c),
            _ => Ty::Any,
        }
    }

    pub fn is_exception_class(&self, c: ClassId) -> bool {
        self.builtin_class("BaseException").map(|b| self.is_subclass(c, b)).unwrap_or(false)
    }

    pub fn builtin_class(&self, name: &str) -> Option<ClassId> {
        match self.modules[self.builtins_mod].scope.get(name) {
            Some(Sym::Class(c)) => Some(*c),
            _ => None,
        }
    }

    fn compile_module_init(&mut self, m: usize) -> R<()> {
        let ast = self.modules[m].ast.clone();
        let mut fx = func::Fx::new(m, None);
        fx.is_module = true;
        fx.ret = Ty::NoneT;
        self.fx_stack.push(fx);
        let r = (|| -> R<()> {
            for st in &ast.body {
                self.stmt(st)?;
            }
            Ok(())
        })();
        let fx = self.fx_stack.pop().unwrap();
        r?;
        let name = self.modules[m].init_cname.clone();
        self.out.protos.push_str(&format!("static void {}(void);\n", name));
        let mut s = format!("static void {}(void) {{\n    static bool done = false;\n    if (done) return;\n    done = true;\n", name);
        for d in &fx.decls {
            s.push_str(&format!("    {}\n", d));
        }
        s.push_str(&fx.body);
        s.push_str("}\n\n");
        self.out.funcs.push_str(&s);
        Ok(())
    }

    fn assemble(&mut self, main: usize) -> String {
        let mut s = String::new();
        s.push_str("/* Generated by fpython. Do not edit. */\n#include \"fpy_runtime.c\"\n");
        if self.uses_python {
            s.push_str("FpyObj *fpy_py_import(const char *name, bool top_level);\nFpyObj *fpy_py_from_import(const char *module, const char *attr);\nvoid fpy_py_publish(const char *name, FpyObj *v);\nFpyObj *fpy_coro_new(FpyIter *gen, const char *name);\nFpyObj *fpy_await(FpyGen *g, FpyObj *aw);\nvoid fpy_py_mark_async(FpyObj *fn);\nFpyObj *fpy_py_native(FpyFunc *f, const char *name, const char *qualname, const char *module, int nparams, const char **pnames, const char **pannots, const char **pdefaults, const char *ret, int is_async, const char *doc);\nFpyObj *fpy_py_make_class(const char *name, const char *module, FpyObj *bases, FpyObj *methods, FpyObj *annots, FpyObj *defaults);\nFpyObj *fpy_py_super(FpyObj *cls, FpyObj *self);\nFpyObj *fpy_py_builtin_wrap(const char *builtin, FpyObj *v);\n");
        }
        s.push('\n');
        // class struct forward decls
        for c in &self.classes {
            s.push_str(&format!("struct {};\n", c.cname));
        }
        for c in &self.classes {
            s.push_str(&format!("extern const FpyType T_{};\n", c.cname));
        }
        s.push_str(&self.out.structs);
        s.push_str("\n/* globals */\nstatic struct {\n    int _dummy;\n");
        for g in &self.globals {
            if let Some(t) = &g.ty {
                s.push_str(&format!("    {} {};\n", self.ctype(t), g.cname));
            }
        }
        s.push_str("} G;\n\n");
        s.push_str(&self.out.strings);
        s.push_str(&self.out.protos);
        s.push('\n');
        s.push_str(&self.out.funcs);
        s.push_str(&self.out.types);
        if self.uses_python {
            s.push_str("\n#ifdef FPY_PYTHON\n");
            s.push_str(&self.extra_c);
            s.push_str("#endif\n");
        }
        // builtin exception factory
        let codes = [
            "Exception", "ValueError", "TypeError", "IndexError", "KeyError", "ZeroDivisionError", "OverflowError", "AttributeError",
            "StopIteration", "RuntimeError", "AssertionError", "NotImplementedError", "OSError", "FileNotFoundError", "RecursionError",
            "MemoryError", "NameError", "ImportError", "UnicodeError", "LookupError", "ArithmeticError", "SystemExit", "KeyboardInterrupt",
            "EOFError", "GeneratorExit",
        ];
        // an empty message means "no argument" (so repr() shows `E()`, as in CPython)
        let no_arg = self.exc_no_arg();
        s.push_str(&format!(
            "FpyObj *fpy_make_builtin_exc(int code, FpyStr *msg) {{\n    FpyObj *arg = msg && msg->ty == &fpy_type_str && msg->len == 0 ? (FpyObj*){} : (FpyObj*)msg;\n    switch (code) {{\n",
            no_arg
        ));
        for (i, n) in codes.iter().enumerate() {
            if let Some(c) = self.builtin_class(n) {
                let cn = &self.classes[c].cname;
                s.push_str(&format!("    case {}: return (FpyObj*)new_{}(arg);\n", i, cn));
            }
        }
        s.push_str("    default: return NULL;\n    }\n}\n");
        s.push_str("const FpyType *fpy_builtin_exc_type(int code) {\n    switch (code) {\n");
        for (i, n) in codes.iter().enumerate() {
            if let Some(c) = self.builtin_class(n) {
                s.push_str(&format!("    case {}: return &T_{};\n", i, self.classes[c].cname));
            }
        }
        s.push_str("    default: return NULL;\n    }\n}\n");
        s.push_str("FpyStr *fpy_exc_message(FpyObj *e) { return fpy_obj_str(e); }\n\n");
        // entry
        s.push_str("static void fpy_entry(void) {\n");
        s.push_str(&format!("    {}();\n", self.modules[self.builtins_mod].init_cname));
        s.push_str(&format!("    {}();\n", self.modules[main].init_cname));
        if self.uses_python {
            s.push_str("}\n\nvoid fpy_py_init(int argc, char **argv);\nvoid fpy_py_finalize(void);\nint main(int argc, char **argv) {\n    int bottom;\n    fpy_rt_init(&bottom, argc, argv);\n    fpy_gc_add_root(&G, sizeof(G));\n    fpy_py_init(argc, argv);\n    if (fpc_install_compiled() < 0) { PyErr_Print(); return 1; }\n    int rc = fpy_run_main(fpy_entry);\n    fpy_py_finalize();\n    return rc;\n}\n");
        } else {
            s.push_str("}\n\nint main(int argc, char **argv) {\n    int bottom;\n    fpy_rt_init(&bottom, argc, argv);\n    fpy_gc_add_root(&G, sizeof(G));\n    return fpy_run_main(fpy_entry);\n}\n");
        }
        s
    }
}

pub fn make_union(ts: Vec<Ty>) -> Ty {
    let mut flat: Vec<Ty> = Vec::new();
    let mut has_none = false;
    fn push(flat: &mut Vec<Ty>, has_none: &mut bool, t: Ty) {
        match t {
            Ty::NoneT => *has_none = true,
            Ty::Optional(i) => {
                *has_none = true;
                push(flat, has_none, *i);
            }
            Ty::Union(ms) => {
                for x in ms {
                    push(flat, has_none, x);
                }
            }
            t => {
                if !flat.contains(&t) {
                    flat.push(t)
                }
            }
        }
    }
    for t in ts {
        push(&mut flat, &mut has_none, t);
    }
    if flat.iter().any(|t| *t == Ty::Any) {
        return Ty::Any;
    }
    let core = match flat.len() {
        0 => return Ty::NoneT,
        1 => flat.pop().unwrap(),
        _ => Ty::Union(flat),
    };
    if has_none {
        Ty::optional(core)
    } else {
        core
    }
}

/// Placeholder element type for empty literals, resolved by context.
pub fn has_unknown(t: &Ty) -> bool {
    match t {
        Ty::Var(n) => n == "?",
        Ty::List(e) | Ty::Set(e) | Ty::Iter(e) | Ty::Optional(e) => has_unknown(e),
        Ty::Dict(k, v) => has_unknown(k) || has_unknown(v),
        Ty::Tuple(ts) => ts.iter().any(has_unknown),
        _ => false,
    }
}

fn is_classvar(ann: &Expr) -> bool {
    match &ann.kind {
        ExprKind::Name(n) => n == "ClassVar",
        ExprKind::Subscript { value, .. } => matches!(&value.kind, ExprKind::Name(n) if n == "ClassVar"),
        _ => false,
    }
}

fn classvar_inner(ann: &Expr) -> Option<&Expr> {
    match &ann.kind {
        ExprKind::Subscript { index, .. } => Some(index),
        _ => None,
    }
}

pub fn looks_like_type_expr(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Subscript { value, .. } => matches!(&value.kind, ExprKind::Name(n) if matches!(n.as_str(), "list" | "dict" | "set" | "tuple" | "Optional" | "Union" | "Callable" | "List" | "Dict" | "Set" | "Tuple" | "Iterator")),
        ExprKind::BinOp { left, op: BinOp::BitOr, right } => {
            let simple = |x: &Expr| match &x.kind {
                ExprKind::Name(n) => matches!(n.as_str(), "int" | "float" | "str" | "bool" | "bytes") || n.chars().next().map(|c| c.is_uppercase()).unwrap_or(false),
                ExprKind::NoneLit => true,
                _ => looks_like_type_expr(x),
            };
            simple(left) && simple(right)
        }
        _ => false,
    }
}

/// Type of the first literal assigned to `name` in a function body (for `global name`).
fn global_literal_type(body: &[Stmt], name: &str) -> Option<Ty> {
    let mut found = None;
    for st in body {
        let mut lit = |v: &Expr| match &v.kind {
            ExprKind::Int(_) => Some(Ty::Int),
            ExprKind::Float(_) => Some(Ty::Float),
            ExprKind::Str { .. } | ExprKind::FStr { .. } => Some(Ty::Str),
            ExprKind::Bool(_) => Some(Ty::Bool),
            ExprKind::Bytes { .. } => Some(Ty::Bytes),
            _ => None,
        };
        match &st.kind {
            StmtKind::Assign { targets, value } if targets.iter().any(|t| matches!(&t.kind, ExprKind::Name(n) if n == name)) => found = lit(value),
            _ => {
                let mut inner = None;
                scope::walk_stmt(st, &mut |node| {
                    if let scope::Node::B(b) = node {
                        if inner.is_none() {
                            inner = global_literal_type(b, name);
                        }
                    }
                });
                found = inner;
            }
        }
        if found.is_some() {
            return found;
        }
    }
    found
}

pub fn collect_assigned(st: &Stmt, out: &mut BTreeSet<String>) {
    match &st.kind {
        StmtKind::Assign { targets, .. } => {
            for t in targets {
                scope::collect_target_names(t, out);
            }
        }
        StmtKind::AnnAssign { target, .. } | StmtKind::AugAssign { target, .. } => scope::collect_target_names(target, out),
        StmtKind::For { target, body, orelse, .. } => {
            scope::collect_target_names(target, out);
            for s in body.iter().chain(orelse) {
                collect_assigned(s, out);
            }
        }
        StmtKind::If { body, orelse, .. } | StmtKind::While { body, orelse, .. } => {
            for s in body.iter().chain(orelse) {
                collect_assigned(s, out);
            }
        }
        StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
            for s in body.iter().chain(orelse).chain(finalbody) {
                collect_assigned(s, out);
            }
            for h in handlers {
                if let Some(n) = &h.name {
                    out.insert(n.clone());
                }
                for s in &h.body {
                    collect_assigned(s, out);
                }
            }
        }
        StmtKind::With { items, body, .. } => {
            for it in items {
                if let Some(v) = &it.var {
                    scope::collect_target_names(v, out);
                }
            }
            for s in body {
                collect_assigned(s, out);
            }
        }
        StmtKind::Match { cases, .. } => {
            for c in cases {
                for s in &c.body {
                    collect_assigned(s, out);
                }
            }
        }
        _ => {}
    }
}

/// Does any method in the class body assign `self.<name>` (directly or augmented)?
/// Whether any statement in `body` (recursively, including nested functions and classes)
/// assigns `<owner>.name` for an owner name accepted by `owner_ok`.
fn assigns_attr(body: &[Stmt], name: &str, owner_ok: &dyn Fn(&str) -> bool) -> bool {
    fn target_hits(t: &Expr, name: &str, owner_ok: &dyn Fn(&str) -> bool) -> bool {
        match &t.kind {
            ExprKind::Attribute { value, attr } => attr == name && matches!(&value.kind, ExprKind::Name(n) if owner_ok(n)),
            ExprKind::Tuple { elts, .. } | ExprKind::List(elts) => elts.iter().any(|e| target_hits(e, name, owner_ok)),
            _ => false,
        }
    }
    body.iter().any(|s| match &s.kind {
        StmtKind::Assign { targets, .. } => targets.iter().any(|t| target_hits(t, name, owner_ok)),
        StmtKind::AugAssign { target, .. } | StmtKind::AnnAssign { target, .. } => target_hits(target, name, owner_ok),
        StmtKind::FuncDef(f) => assigns_attr(&f.body, name, owner_ok),
        StmtKind::ClassDef(c) => assigns_attr(&c.body, name, owner_ok),
        StmtKind::If { body, orelse, .. } | StmtKind::While { body, orelse, .. } | StmtKind::For { body, orelse, .. } => {
            assigns_attr(body, name, owner_ok) || assigns_attr(orelse, name, owner_ok)
        }
        StmtKind::With { body, .. } => assigns_attr(body, name, owner_ok),
        StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
            assigns_attr(body, name, owner_ok) || assigns_attr(orelse, name, owner_ok) || assigns_attr(finalbody, name, owner_ok) || handlers.iter().any(|h| assigns_attr(&h.body, name, owner_ok))
        }
        StmtKind::Match { cases, .. } => cases.iter().any(|c| assigns_attr(&c.body, name, owner_ok)),
        _ => false,
    })
}

fn collect_self_assigns(body: &[Stmt], out: &mut Vec<(String, Option<Ty>, Span, Option<Expr>)>) {
    for st in body {
        match &st.kind {
            StmtKind::Assign { targets, value } => {
                for t in targets {
                    if let ExprKind::Attribute { value: v, attr } = &t.kind {
                        if matches!(&v.kind, ExprKind::Name(n) if n == "self") && !out.iter().any(|(n, ..)| n == attr) {
                            out.push((attr.clone(), None, t.span, Some(value.clone())));
                        }
                    }
                }
            }
            StmtKind::AnnAssign { target, ann, value } => {
                if let ExprKind::Attribute { value: v, attr } = &target.kind {
                    if matches!(&v.kind, ExprKind::Name(n) if n == "self") {
                        out.retain(|(n, ..)| n != attr);
                        // Annotation is resolved later; store as a marker via value None + special.
                        out.push((attr.clone(), None, target.span, Some(Expr::new(ExprKind::Call { func: Box::new(Expr::new(ExprKind::Name("__ann__".into()), ann.span)), args: vec![ann.clone()], keywords: vec![] }, ann.span))));
                        let _ = value;
                    }
                }
            }
            StmtKind::If { body, orelse, .. } | StmtKind::While { body, orelse, .. } | StmtKind::For { body, orelse, .. } => {
                collect_self_assigns(body, out);
                collect_self_assigns(orelse, out);
            }
            StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
                collect_self_assigns(body, out);
                for h in handlers {
                    collect_self_assigns(&h.body, out);
                }
                collect_self_assigns(orelse, out);
                collect_self_assigns(finalbody, out);
            }
            StmtKind::With { body, .. } => collect_self_assigns(body, out),
            _ => {}
        }
    }
}
