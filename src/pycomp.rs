//! Native compilation of pure-Python modules (the first stage of the Python-library fallback).
//!
//! A `.py` module is compiled to C that manipulates CPython objects directly through the C API
//! (like Cython or Nuitka): no bytecode is interpreted, but semantics are exactly Python's because
//! the objects are real Python objects. Every expression produces a *new reference* in a temporary;
//! errors jump through a per-function label pointer (`goto *__err`) so try/except/finally map onto
//! C control flow. Constructs that cannot be compiled faithfully (generators, async, `match`, ...)
//! make `compile_module` return an error, and the module is then imported by CPython as usual.

use crate::ast::*;
use crate::compiler::scope;
use crate::compiler::{c_ident, c_string};
use crate::parser;
use std::collections::{BTreeSet, HashMap};

pub struct CompiledModule {
    pub name: String,
    pub c_code: String,
    pub exec_fn: String,
    pub is_package: bool,
}

type R<T> = Result<T, String>;

#[derive(Clone)]
enum LVar {
    Fast(String),
    Cell(String),
    Free(usize),
}

#[derive(Clone)]
enum Fin {
    Stmts(Vec<Stmt>),
    Raw(String),
}

#[derive(Clone)]
struct LoopCtx {
    fin_depth: usize,
    broke: Option<String>,
}

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    Module,
    Class,
    Function,
}

struct Frame {
    kind: Kind,
    body: String,
    indent: usize,
    temps: Vec<String>,
    hidden: Vec<String>,
    locals: HashMap<String, LVar>,
    globals_decl: BTreeSet<String>,
    ns: Option<String>,
    loops: Vec<LoopCtx>,
    fins: Vec<Fin>,
    comp: Vec<HashMap<String, String>>,
    free_names: Vec<String>,
    labels: usize,
    first_arg: Option<String>,
    class_cell: Option<String>,
    /// a class body (possibly nested) running inside a function: it can read the function's locals
    class_in_fn: bool,
    is_gen: bool,
    is_coro: bool,
}

impl Frame {
    fn new(kind: Kind) -> Frame {
        Frame {
            kind,
            body: String::new(),
            indent: 1,
            temps: Vec::new(),
            hidden: Vec::new(),
            locals: HashMap::new(),
            globals_decl: BTreeSet::new(),
            ns: None,
            loops: Vec::new(),
            fins: Vec::new(),
            comp: Vec::new(),
            free_names: Vec::new(),
            labels: 0,
            first_arg: None,
            class_cell: None,
            class_in_fn: false,
            is_gen: false,
            is_coro: false,
        }
    }
}

pub struct PyComp {
    next_is_async: bool,
    /// `__qualname__` prefix of the scope being compiled ("", "C.", "f.<locals>.")
    qual_prefix: String,
    prefix: String,
    path: String,
    consts: Vec<(String, String)>,
    const_map: HashMap<String, String>,
    funcs: String,
    protos: String,
    nfn: usize,
    frames: Vec<Frame>,
    tmpn: usize,
}

/// Builtins that consume an iterable eagerly; `f(x for x in y)` compiles to `f([x for x in y])`.
const EAGER_CONSUMERS: &[&str] = &["list", "tuple", "set", "frozenset", "sorted", "sum", "min", "max", "dict"];

pub fn compile_module(name: &str, path: &str, src: &str) -> R<CompiledModule> {
    let parsed = parser::parse(src, path).map_err(|d| format!("{}", d))?;
    let mut pc = PyComp {
        next_is_async: false,
        qual_prefix: String::new(),
        prefix: format!("pym_{}", c_ident(name)),
        path: path.to_string(),
        consts: Vec::new(),
        const_map: HashMap::new(),
        funcs: String::new(),
        protos: String::new(),
        nfn: 0,
        frames: vec![Frame::new(Kind::Module)],
        tmpn: 0,
    };
    pc.check_supported(&parsed.module.body)?;
    for st in &parsed.module.body {
        pc.stmt(st)?;
    }
    let f = pc.frames.pop().unwrap();
    let exec_fn = format!("{}_exec", pc.prefix);
    let mut c = String::new();
    c.push_str(&format!("/* compiled from {} */\n", path));
    for (n, _) in &pc.consts {
        c.push_str(&format!("static PyObject *{};\n", n));
    }
    c.push_str(&pc.protos);
    c.push_str(&format!("static int {}_consts(void) {{\n    static int done;\n    if (done) return 0;\n    done = 1;\n", pc.prefix));
    for (n, init) in &pc.consts {
        c.push_str(&format!("    {} = {};\n    if (!{}) return -1;\n", n, init, n));
    }
    c.push_str("    return 0;\n}\n\n");
    c.push_str(&pc.funcs);
    c.push_str(&format!("int {}(PyObject *__module) {{\n    if ({}_consts() < 0) return -1;\n    PyObject *__globals = PyModule_GetDict(__module);\n    void *__err = &&L_err;\n", exec_fn, pc.prefix));
    for t in f.temps.iter().chain(f.hidden.iter()) {
        c.push_str(&format!("    PyObject *{} = NULL;\n", t));
    }
    c.push_str(&expand_clears(&f.body, &f));
    let clear = expand_clears("@@CLEAR_TEMPS:0@@", &f);
    c.push_str("    goto L_ok;\nL_err:\n");
    c.push_str(&format!("    {}\n    return -1;\nL_ok:\n    {}\n    return 0;\n}}\n\n", clear, clear));
    Ok(CompiledModule { name: name.to_string(), c_code: c, exec_fn, is_package: path.ends_with("__init__.py") })
}

/// Expand `@@CLEAR_TEMPS:n@@` markers: an exception handler releases every expression temp and
/// the hidden variables created inside its protected body (index >= n), but not hidden variables
/// of enclosing statements that are still live (a `for` iterator, a `with` manager, ...).
fn expand_clears(body: &str, f: &Frame) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(i) = rest.find("@@CLEAR_TEMPS:") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 14..];
        let j = after.find("@@").unwrap();
        let from: usize = after[..j].parse().unwrap();
        let names: Vec<String> = f.temps.iter().chain(f.hidden.iter().skip(from)).map(|t| format!("Py_CLEAR({});", t)).collect();
        out.push_str(&names.join(" "));
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    out
}

impl PyComp {
    fn fr(&mut self) -> &mut Frame {
        self.frames.last_mut().unwrap()
    }
    fn frr(&self) -> &Frame {
        self.frames.last().unwrap()
    }

    fn emit(&mut self, line: &str) {
        let f = self.fr();
        for _ in 0..f.indent {
            f.body.push_str("    ");
        }
        f.body.push_str(line);
        f.body.push('\n');
    }

    fn temp(&mut self) -> String {
        self.tmpn += 1;
        let t = format!("t{}", self.tmpn);
        self.fr().temps.push(t.clone());
        t
    }

    fn hidden(&mut self, prefix: &str) -> String {
        self.tmpn += 1;
        let t = format!("h_{}{}", prefix, self.tmpn);
        self.fr().hidden.push(t.clone());
        t
    }

    fn label(&mut self, p: &str) -> String {
        self.tmpn += 1;
        format!("L_{}{}", p, self.tmpn)
    }

    /// Emit `t = <code>; if (!t) goto *__err;` and return t.
    fn call_new(&mut self, code: &str) -> String {
        let t = self.temp();
        self.emit(&format!("{} = {};", t, code));
        self.emit(&format!("if (!{}) goto *__err;", t));
        t
    }

    fn check_int(&mut self, code: &str) {
        self.emit(&format!("if (({}) < 0) goto *__err;", code));
    }

    fn release(&mut self, t: &str) {
        self.emit(&format!("Py_CLEAR({});", t));
    }

    // ------------------------------------------------------------------ constants

    fn konst(&mut self, key: String, init: String) -> String {
        if let Some(n) = self.const_map.get(&key) {
            return n.clone();
        }
        let n = format!("{}_k{}", self.prefix, self.consts.len());
        self.consts.push((n.clone(), init));
        self.const_map.insert(key, n.clone());
        n
    }

    fn name_const(&mut self, s: &str) -> String {
        self.konst(format!("n:{}", s), format!("PyUnicode_InternFromString({})", c_string(s.as_bytes())))
    }

    fn str_const(&mut self, s: &str) -> String {
        self.konst(format!("s:{}", s), format!("PyUnicode_DecodeUTF8({}, {}, \"surrogatepass\")", c_string(s.as_bytes()), s.len()))
    }

    // ------------------------------------------------------------------ support check

    fn check_supported(&self, body: &[Stmt]) -> R<()> {
        for st in body {
            match &st.kind {
                StmtKind::Match { .. } => return Err(format!("line {}: match statement", st.span.line)),
                StmtKind::FuncDef(f) => {
                    if f.is_async && scope::analyze_function(&f.params, &f.body).has_yield {
                        return Err(format!("line {}: async generator '{}'", st.span.line, f.name));
                    }
                    self.check_supported(&f.body)?;
                }
                StmtKind::ClassDef(c) => self.check_supported(&c.body)?,

                StmtKind::Try { star: true, .. } => return Err(format!("line {}: except*", st.span.line)),
                _ => {}
            }
            let mut err = None;
            let mut nodes = Vec::new();
            scope::walk_stmt(st, &mut |n| nodes.push(n));
            for n in nodes {
                match n {
                    scope::Node::E(e) => check_expr(e, &mut err, false),
                    scope::Node::B(b) => self.check_supported(b)?,
                }
            }
            if let Some(e) = err {
                return Err(format!("line {}: {}", st.span.line, e));
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ names

    fn globals_var(&self) -> &'static str {
        "__globals"
    }

    fn load_name(&mut self, n: &str) -> R<String> {
        for i in (0..self.frr().comp.len()).rev() {
            if let Some(v) = self.frr().comp[i].get(n).cloned() {
                let t = self.temp();
                self.emit(&format!("{} = Py_NewRef({});", t, v));
                return Ok(t);
            }
        }
        let kind = self.frr().kind;
        if kind == Kind::Function && !self.frr().globals_decl.contains(n) {
            if let Some(lv) = self.frr().locals.get(n).cloned() {
                let t = self.temp();
                match lv {
                    LVar::Fast(c) => {
                        self.emit(&format!("if (!{}) {{ fpc_unbound_local({}); goto *__err; }}", c, c_string(n.as_bytes())));
                        self.emit(&format!("{} = Py_NewRef({});", t, c));
                    }
                    LVar::Cell(c) => {
                        self.emit(&format!("{} = Py_XNewRef(PyCell_GET({}));", t, c));
                        self.emit(&format!("if (!{}) {{ fpc_unbound_local({}); goto *__err; }}", t, c_string(n.as_bytes())));
                    }
                    LVar::Free(i) => {
                        self.emit(&format!("{} = Py_XNewRef(PyCell_GET(fpc_closure_cell(__self, {})));", t, i));
                        self.emit(&format!("if (!{}) {{ PyErr_Format(PyExc_NameError, \"free variable '%s' referenced before assignment\", {}); goto *__err; }}", t, c_string(n.as_bytes())));
                    }
                }
                return Ok(t);
            }
        }
        if n == "__class__" {
            if let Some(cc) = self.frr().class_cell.clone() {
                let t = self.temp();
                self.emit(&format!("{} = Py_XNewRef(PyCell_GET({}));", t, cc));
                return Ok(t);
            }
        }
        let nk = self.name_const(n);
        if kind == Kind::Class {
            let ns = self.frr().ns.clone().unwrap();
            if self.frr().class_in_fn {
                if let Some(lv) = self.frr().locals.get(n).cloned() {
                    // class namespace first, then the enclosing function's variable
                    let t = self.temp();
                    let local = match lv {
                        LVar::Fast(c) => c,
                        LVar::Cell(c) => format!("PyCell_GET({})", c),
                        LVar::Free(i) => format!("PyCell_GET(fpc_closure_cell(__self, {}))", i),
                    };
                    self.emit(&format!("if (PyMapping_GetOptionalItem({}, {}, &{}) < 0) goto *__err;", ns, nk, t));
                    self.emit(&format!(
                        "if (!{t}) {{ {t} = Py_XNewRef({local}); if (!{t}) {{ PyErr_Format(PyExc_NameError, \"cannot access free variable '%s'\", {name}); goto *__err; }} }}",
                        t = t,
                        local = local,
                        name = c_string(n.as_bytes())
                    ));
                    return Ok(t);
                }
            }
            return Ok(self.call_new(&format!("fpc_load_name({}, __globals, {})", ns, nk)));
        }
        Ok(self.call_new(&format!("fpc_load_global(__globals, {})", nk)))
    }

    /// Store (stealing reference `v`).
    fn store_name(&mut self, n: &str, v: &str) -> R<()> {
        for i in (0..self.frr().comp.len()).rev() {
            if let Some(c) = self.frr().comp[i].get(n).cloned() {
                self.emit(&format!("Py_XSETREF({}, {}); {} = NULL;", c, v, v));
                return Ok(());
            }
        }
        let kind = self.frr().kind;
        if kind == Kind::Function && !self.frr().globals_decl.contains(n) {
            if let Some(lv) = self.frr().locals.get(n).cloned() {
                match lv {
                    LVar::Fast(c) => self.emit(&format!("Py_XSETREF({}, {}); {} = NULL;", c, v, v)),
                    LVar::Cell(c) => self.emit(&format!("PyCell_Set({}, {}); Py_CLEAR({});", c, v, v)),
                    LVar::Free(i) => self.emit(&format!("PyCell_Set(fpc_closure_cell(__self, {}), {}); Py_CLEAR({});", i, v, v)),
                }
                return Ok(());
            }
        }
        let nk = self.name_const(n);
        if kind == Kind::Class {
            let ns = self.frr().ns.clone().unwrap();
            self.check_int(&format!("PyObject_SetItem({}, {}, {})", ns, nk, v));
        } else {
            self.check_int(&format!("PyDict_SetItem(__globals, {}, {})", nk, v));
        }
        self.release(v);
        Ok(())
    }

    fn delete_name(&mut self, n: &str) -> R<()> {
        let kind = self.frr().kind;
        if kind == Kind::Function && !self.frr().globals_decl.contains(n) {
            if let Some(LVar::Fast(c)) = self.frr().locals.get(n).cloned() {
                self.emit(&format!("if (!{}) {{ fpc_unbound_local({}); goto *__err; }}", c, c_string(n.as_bytes())));
                self.emit(&format!("Py_CLEAR({});", c));
                return Ok(());
            }
            if let Some(LVar::Cell(c)) = self.frr().locals.get(n).cloned() {
                self.emit(&format!("PyCell_Set({}, NULL);", c));
                return Ok(());
            }
        }
        let nk = self.name_const(n);
        if kind == Kind::Class {
            let ns = self.frr().ns.clone().unwrap();
            self.check_int(&format!("PyObject_DelItem({}, {})", ns, nk));
        } else {
            self.check_int(&format!("PyDict_DelItem(__globals, {})", nk));
        }
        Ok(())
    }

    // ------------------------------------------------------------------ statements

    fn block(&mut self, b: &[Stmt]) -> R<()> {
        for s in b {
            self.stmt(s)?;
        }
        Ok(())
    }

    fn stmt(&mut self, st: &Stmt) -> R<()> {
        match &st.kind {
            StmtKind::Pass => Ok(()),
            StmtKind::Expr(e) => {
                let t = self.expr(e)?;
                self.release(&t);
                Ok(())
            }
            StmtKind::Assign { targets, value } => {
                let v = self.expr(value)?;
                for (i, t) in targets.iter().enumerate() {
                    if i + 1 < targets.len() {
                        let dup = self.temp();
                        self.emit(&format!("{} = Py_NewRef({});", dup, v));
                        self.assign(t, &dup)?;
                    } else {
                        self.assign(t, &v)?;
                    }
                }
                Ok(())
            }
            StmtKind::AnnAssign { target, ann, value } => {
                if self.frr().kind == Kind::Class {
                    if let ExprKind::Name(n) = &target.kind {
                        // record the annotation (as a string, like `from __future__ import annotations`)
                        let src = std::fs::read_to_string(&self.path).unwrap_or_default();
                        let text = src.get(ann.span.start..ann.span.end).unwrap_or("object").to_string();
                        let ns = self.frr().ns.clone().unwrap();
                        let ak = self.name_const("__annotations__");
                        let d = self.temp();
                        self.emit(&format!("{} = PyObject_GetItem({}, {});", d, ns, ak));
                        self.emit(&format!("if (!{d}) {{ PyErr_Clear(); {d} = PyDict_New(); if (!{d} || PyObject_SetItem({ns}, {ak}, {d}) < 0) goto *__err; }}", d = d, ns = ns, ak = ak));
                        let sk = self.str_const(&text);
                        let nk = self.name_const(n);
                        self.check_int(&format!("PyObject_SetItem({}, {}, {})", d, nk, sk));
                        self.release(&d);
                    }
                }
                if let Some(v) = value {
                    let t = self.expr(v)?;
                    self.assign(target, &t)?;
                }
                Ok(())
            }
            StmtKind::AugAssign { target, op, value } => self.aug_assign(target, *op, value),
            StmtKind::Return(v) => {
                let t = match v {
                    Some(e) => self.expr(e)?,
                    None => {
                        let t = self.temp();
                        self.emit(&format!("{} = Py_NewRef(Py_None);", t));
                        t
                    }
                };
                // keep the value across finally blocks
                let rv = self.hidden("ret");
                self.emit(&format!("{} = {}; {} = NULL;", rv, t, t));
                self.unwind_fins(0)?;
                self.emit(&format!("__ret = {}; {} = NULL; goto L_ret;", rv, rv));
                Ok(())
            }
            StmtKind::If { test, body, orelse } => {
                let c = self.truth(test)?;
                self.emit(&format!("if ({}) {{", c));
                self.fr().indent += 1;
                self.block(body)?;
                self.fr().indent -= 1;
                if !orelse.is_empty() {
                    self.emit("} else {");
                    self.fr().indent += 1;
                    self.block(orelse)?;
                    self.fr().indent -= 1;
                }
                self.emit("}");
                Ok(())
            }
            StmtKind::While { test, body, orelse } => {
                let broke = if orelse.is_empty() { None } else { Some(self.int_var("broke")) };
                self.emit("for (;;) {");
                self.fr().indent += 1;
                let c = self.truth(test)?;
                self.emit(&format!("if (!{}) break;", c));
                let depth = self.frr().fins.len();
                self.fr().loops.push(LoopCtx { fin_depth: depth, broke: broke.clone() });
                self.block(body)?;
                self.fr().loops.pop();
                self.fr().indent -= 1;
                self.emit("}");
                if let Some(b) = broke {
                    self.emit(&format!("if (!{}) {{", b));
                    self.fr().indent += 1;
                    self.block(orelse)?;
                    self.fr().indent -= 1;
                    self.emit("}");
                }
                Ok(())
            }
            StmtKind::For { target, iter, body, orelse, is_async } => {
                if *is_async && !self.frr().is_coro {
                    return Err("'async for' outside async function".into());
                }
                let broke = if orelse.is_empty() { None } else { Some(self.int_var("broke")) };
                let x = self.expr(iter)?;
                let it = self.hidden("it");
                if *is_async {
                    let k = self.name_const("__aiter__");
                    self.emit(&format!("{} = PyObject_CallMethodNoArgs({}, {}); Py_CLEAR({});", it, x, k, x));
                } else {
                    self.emit(&format!("{} = PyObject_GetIter({}); Py_CLEAR({});", it, x, x));
                }
                self.emit(&format!("if (!{}) goto *__err;", it));
                self.emit("for (;;) {");
                self.fr().indent += 1;
                let item = self.temp();
                if *is_async {
                    let k = self.name_const("__anext__");
                    self.emit(&format!(
                        "{{ PyObject *_aw = PyObject_CallMethodNoArgs({it}, {k}); if (!_aw) goto *__err; {item} = fpc_await(__gen, _aw); Py_DECREF(_aw); }}"
                    ));
                    self.emit(&format!(
                        "if (!{}) {{ if (!PyErr_ExceptionMatches(PyExc_StopAsyncIteration)) goto *__err; PyErr_Clear(); break; }}",
                        item
                    ));
                } else {
                    self.emit(&format!("{} = PyIter_Next({});", item, it));
                    self.emit(&format!("if (!{}) {{ if (PyErr_Occurred()) goto *__err; break; }}", item));
                }
                self.assign(target, &item)?;
                let depth = self.frr().fins.len();
                self.fr().loops.push(LoopCtx { fin_depth: depth, broke: broke.clone() });
                self.block(body)?;
                self.fr().loops.pop();
                self.fr().indent -= 1;
                self.emit("}");
                self.emit(&format!("Py_CLEAR({});", it));
                if let Some(b) = broke {
                    self.emit(&format!("if (!{}) {{", b));
                    self.fr().indent += 1;
                    self.block(orelse)?;
                    self.fr().indent -= 1;
                    self.emit("}");
                }
                Ok(())
            }
            StmtKind::Break => {
                let lp = self.frr().loops.last().cloned().ok_or("'break' outside loop")?;
                self.unwind_fins(lp.fin_depth)?;
                if let Some(b) = lp.broke {
                    self.emit(&format!("{} = 1;", b));
                }
                self.emit("break;");
                Ok(())
            }
            StmtKind::Continue => {
                let lp = self.frr().loops.last().cloned().ok_or("'continue' outside loop")?;
                self.unwind_fins(lp.fin_depth)?;
                self.emit("continue;");
                Ok(())
            }
            StmtKind::Raise { exc, cause } => {
                let e = match exc {
                    Some(e) => Some(self.expr(e)?),
                    None => None,
                };
                let c = match cause {
                    Some(c) => Some(self.expr(c)?),
                    None => None,
                };
                self.emit(&format!("fpc_raise({}, {});", e.clone().unwrap_or_else(|| "NULL".into()), c.clone().unwrap_or_else(|| "NULL".into())));
                self.emit("goto *__err;");
                Ok(())
            }
            StmtKind::Assert { test, msg } => {
                let c = self.truth(test)?;
                self.emit(&format!("if (!{}) {{", c));
                self.fr().indent += 1;
                match msg {
                    Some(m) => {
                        let t = self.expr(m)?;
                        self.emit(&format!("PyErr_SetObject(PyExc_AssertionError, {});", t));
                    }
                    None => self.emit("PyErr_SetNone(PyExc_AssertionError);"),
                }
                self.emit("goto *__err;");
                self.fr().indent -= 1;
                self.emit("}");
                Ok(())
            }
            StmtKind::Del(targets) => {
                for t in targets {
                    match &t.kind {
                        ExprKind::Name(n) => self.delete_name(n)?,
                        ExprKind::Attribute { value, attr } => {
                            let o = self.expr(value)?;
                            let nk = self.name_const(attr);
                            self.check_int(&format!("PyObject_DelAttr({}, {})", o, nk));
                            self.release(&o);
                        }
                        ExprKind::Subscript { value, index } => {
                            let o = self.expr(value)?;
                            let k = self.index_expr(index)?;
                            self.check_int(&format!("PyObject_DelItem({}, {})", o, k));
                            self.release(&o);
                            self.release(&k);
                        }
                        ExprKind::Tuple { elts, .. } | ExprKind::List(elts) => {
                            let st2 = Stmt { kind: StmtKind::Del(elts.clone()), span: st.span, end_line: st.end_line, header_end: st.header_end };
                            self.stmt(&st2)?;
                        }
                        _ => return Err("unsupported del target".into()),
                    }
                }
                Ok(())
            }
            StmtKind::Global(ns) => {
                for n in ns {
                    self.fr().globals_decl.insert(n.clone());
                }
                Ok(())
            }
            StmtKind::Nonlocal(_) => Ok(()),
            StmtKind::Import(names) => {
                for a in names {
                    let level = 0;
                    let m = self.call_new(&format!("fpc_import(__globals, {}, NULL, {})", c_string(a.name.as_bytes()), level));
                    match &a.asname {
                        Some(asn) => {
                            // `import a.b as c` binds the submodule
                            let mut cur = m.clone();
                            for part in a.name.split('.').skip(1) {
                                let nk = self.name_const(part);
                                let n2 = self.call_new(&format!("PyObject_GetAttr({}, {})", cur, nk));
                                self.release(&cur);
                                cur = n2;
                            }
                            self.store_name(asn, &cur)?;
                        }
                        None => {
                            let top = a.name.split('.').next().unwrap().to_string();
                            self.store_name(&top, &m)?;
                        }
                    }
                }
                Ok(())
            }
            StmtKind::ImportFrom { module, level, names } => {
                let star = names.iter().any(|a| a.name == "*");
                let fl = if star {
                    let s = self.str_const("*");
                    self.call_new(&format!("PyTuple_Pack(1, {})", s))
                } else {
                    let items: Vec<String> = names.iter().map(|a| self.str_const(&a.name)).collect();
                    self.call_new(&format!("PyTuple_Pack({}, {})", items.len(), items.join(", ")))
                };
                let m = self.call_new(&format!("fpc_import(__globals, {}, {}, {})", c_string(module.as_bytes()), fl, level));
                self.release(&fl);
                if star {
                    let target = if self.frr().kind == Kind::Class { self.frr().ns.clone().unwrap() } else { "__globals".into() };
                    self.check_int(&format!("fpc_import_star({}, {})", m, target));
                } else {
                    for a in names {
                        let nk = self.name_const(&a.name);
                        let v = self.call_new(&format!("fpc_import_from({}, {})", m, nk));
                        let bind = a.asname.clone().unwrap_or_else(|| a.name.clone());
                        self.store_name(&bind, &v)?;
                    }
                }
                self.release(&m);
                Ok(())
            }
            StmtKind::Try { body, handlers, orelse, finalbody, .. } => self.try_stmt(body, handlers, orelse, finalbody),
            StmtKind::With { items, body, is_async } => {
                if *is_async && !self.frr().is_coro {
                    return Err("'async with' outside async function".into());
                }
                self.with_stmt(items, body, *is_async)
            }
            StmtKind::FuncDef(f) => {
                let fv = self.make_function(f)?;
                self.store_name(&f.name, &fv)
            }
            StmtKind::ClassDef(c) => self.class_def(c),
            StmtKind::TypeAlias { .. } => Err("type alias statement".into()),
            StmtKind::Match { .. } => Err("match statement".into()),
        }
    }

    fn int_var(&mut self, p: &str) -> String {
        self.tmpn += 1;
        let n = format!("i_{}{}", p, self.tmpn);
        // declared as a hidden "object" slot is wrong for ints: emit a block-scoped int
        self.emit(&format!("int {} = 0;", n));
        n
    }

    /// Inline pending finally/with-exit code when jumping out (return/break/continue).
    fn unwind_fins(&mut self, depth: usize) -> R<()> {
        let fins: Vec<Fin> = self.frr().fins[depth..].to_vec();
        for (i, f) in fins.iter().enumerate().rev() {
            let saved: Vec<Fin> = self.fr().fins.drain(depth + i..).collect();
            let r = match f {
                Fin::Stmts(b) => self.block(b),
                Fin::Raw(code) => {
                    for line in code.lines() {
                        self.emit(line);
                    }
                    Ok(())
                }
            };
            self.fr().fins.extend(saved);
            r?;
        }
        Ok(())
    }

    fn assign(&mut self, t: &Expr, v: &str) -> R<()> {
        match &t.kind {
            ExprKind::Name(n) => self.store_name(n, v),
            ExprKind::Attribute { value, attr } => {
                let o = self.expr(value)?;
                let nk = self.name_const(attr);
                self.check_int(&format!("PyObject_SetAttr({}, {}, {})", o, nk, v));
                self.release(&o);
                self.release(v);
                Ok(())
            }
            ExprKind::Subscript { value, index } => {
                let o = self.expr(value)?;
                let k = self.index_expr(index)?;
                self.check_int(&format!("PyObject_SetItem({}, {}, {})", o, k, v));
                self.release(&o);
                self.release(&k);
                self.release(v);
                Ok(())
            }
            ExprKind::Tuple { elts, .. } | ExprKind::List(elts) => {
                let star = elts.iter().position(|e| matches!(e.kind, ExprKind::Starred(_)));
                let (before, after) = match star {
                    Some(s) => (s, elts.len() - s - 1),
                    None => (elts.len(), 0),
                };
                let l = self.call_new(&format!("fpc_unpack({}, {}, {}, {})", v, before, star.is_some() as i32, after));
                self.release(v);
                for (i, e) in elts.iter().enumerate() {
                    let item = self.temp();
                    self.emit(&format!("{} = Py_NewRef(PyList_GET_ITEM({}, {}));", item, l, i));
                    let target = match &e.kind {
                        ExprKind::Starred(x) => &**x,
                        _ => e,
                    };
                    self.assign(target, &item)?;
                }
                self.release(&l);
                Ok(())
            }
            ExprKind::Starred(x) => self.assign(x, v),
            _ => Err("unsupported assignment target".into()),
        }
    }

    fn aug_assign(&mut self, target: &Expr, op: BinOp, value: &Expr) -> R<()> {
        let f = inplace_fn(op);
        match &target.kind {
            ExprKind::Name(n) => {
                let cur = self.load_name(n)?;
                let r = self.expr(value)?;
                let res = self.call_new(&format!("{}({}, {})", f, cur, r));
                self.release(&cur);
                self.release(&r);
                self.store_name(n, &res)
            }
            ExprKind::Attribute { value: obj, attr } => {
                let o = self.expr(obj)?;
                let nk = self.name_const(attr);
                let cur = self.call_new(&format!("PyObject_GetAttr({}, {})", o, nk));
                let r = self.expr(value)?;
                let res = self.call_new(&format!("{}({}, {})", f, cur, r));
                self.check_int(&format!("PyObject_SetAttr({}, {}, {})", o, nk, res));
                for t in [o, cur, r, res] {
                    self.release(&t);
                }
                Ok(())
            }
            ExprKind::Subscript { value: obj, index } => {
                let o = self.expr(obj)?;
                let k = self.index_expr(index)?;
                let cur = self.call_new(&format!("PyObject_GetItem({}, {})", o, k));
                let r = self.expr(value)?;
                let res = self.call_new(&format!("{}({}, {})", f, cur, r));
                self.check_int(&format!("PyObject_SetItem({}, {}, {})", o, k, res));
                for t in [o, k, cur, r, res] {
                    self.release(&t);
                }
                Ok(())
            }
            _ => Err("unsupported augmented assignment target".into()),
        }
    }

    fn try_stmt(&mut self, body: &[Stmt], handlers: &[ExceptHandler], orelse: &[Stmt], finalbody: &[Stmt]) -> R<()> {
        let saved = format!("__saved_err{}", self.tmpn + 1);
        self.tmpn += 1;
        self.emit(&format!("{{ void *{} = __err;", saved));
        self.fr().indent += 1;
        let lf = self.label("fin");
        let lfend = self.label("finend");
        let fmark = self.frr().hidden.len();
        if !finalbody.is_empty() {
            self.emit(&format!("__err = &&{};", lf));
            self.fr().fins.push(Fin::Stmts(finalbody.to_vec()));
        }
        if handlers.is_empty() {
            self.block(body)?;
            self.block(orelse)?;
        } else {
            let lh = self.label("h");
            let lend = self.label("hend");
            let inner_saved = format!("__saved_err{}", self.tmpn + 1);
            self.tmpn += 1;
            self.emit(&format!("{{ void *{} = __err; __err = &&{};", inner_saved, lh));
            self.fr().indent += 1;
            let hmark = self.frr().hidden.len();
            self.block(body)?;
            self.emit(&format!("__err = {};", inner_saved));
            self.block(orelse)?;
            self.emit(&format!("goto {};", lend));
            self.fr().indent -= 1;
            self.emit(&format!("{}:", lh));
            self.fr().indent += 1;
            self.emit(&format!("__err = {};", inner_saved));
            self.emit(&format!("@@CLEAR_TEMPS:{}@@", hmark));
            let exc = self.hidden("exc");
            let prev = self.hidden("prev");
            self.emit(&format!("{} = PyErr_GetRaisedException();", exc));
            self.emit(&format!("{} = PyErr_GetHandledException(); PyErr_SetHandledException({});", prev, exc));
            // restore the previously handled exception when leaving the handler (normally or not)
            let restore = format!("PyErr_SetHandledException({p}); Py_CLEAR({p}); Py_CLEAR({e});", p = prev, e = exc);
            let lhe = self.label("herr");
            let hsaved = format!("__saved_err{}", self.tmpn + 1);
            self.tmpn += 1;
            self.emit(&format!("{{ void *{} = __err; __err = &&{};", hsaved, lhe));
            self.fr().fins.push(Fin::Raw(restore.clone()));
            let mut catch_all = false;
            let mut opened = 0;
            for (hi, h) in handlers.iter().enumerate() {
                if hi > 0 {
                    // test for the next clause lives in the previous clause's else-branch
                    self.emit("else {");
                    self.fr().indent += 1;
                    opened += 1;
                }
                let cond = match &h.typ {
                    None => {
                        catch_all = true;
                        "1".to_string()
                    }
                    Some(t) => {
                        let cls = self.expr(t)?;
                        let m = self.int_var("m");
                        self.emit(&format!("{} = fpc_exc_matches({}, {}); Py_CLEAR({});", m, exc, cls, cls));
                        m
                    }
                };
                self.emit(&format!("if ({}) {{", cond));
                self.fr().indent += 1;
                if let Some(n) = &h.name {
                    let v = self.temp();
                    self.emit(&format!("{} = Py_NewRef({});", v, exc));
                    self.store_name(n, &v)?;
                }
                self.block(&h.body)?;
                if let Some(n) = &h.name {
                    // Python deletes the name at the end of the handler
                    if self.frr().kind == Kind::Function {
                        if let Some(LVar::Fast(c)) = self.frr().locals.get(n).cloned() {
                            self.emit(&format!("Py_CLEAR({});", c));
                        }
                    }
                }
                self.fr().indent -= 1;
                self.emit("}");
            }
            if !catch_all {
                self.emit(&format!("else {{ __err = {hs}; PyErr_SetHandledException({p}); Py_CLEAR({p}); PyErr_SetRaisedException({e}); {e} = NULL; goto *__err; }}", hs = hsaved, p = prev, e = exc));
            }
            for _ in 0..opened {
                self.fr().indent -= 1;
                self.emit("}");
            }
            self.fr().fins.pop();
            self.emit(&format!("__err = {};", hsaved));
            self.emit(&restore);
            self.emit(&format!("goto {};", lend));
            // an exception raised inside a handler: restore handled state, chain, and propagate
            self.emit(&format!("{}:", lhe));
            self.emit(&format!("__err = {};", hsaved));
            self.emit(&format!("PyErr_SetHandledException({p}); Py_CLEAR({p}); Py_CLEAR({e}); goto *__err;", p = prev, e = exc));
            self.emit("}");
            self.fr().indent -= 1;
            self.emit(&format!("{}: ;", lend));
            self.emit("}");
        }
        if !finalbody.is_empty() {
            self.fr().fins.pop();
            self.emit(&format!("__err = {};", saved));
            self.block(finalbody)?;
            self.emit(&format!("goto {};", lfend));
            self.emit(&format!("{}:", lf));
            self.emit(&format!("__err = {};", saved));
            self.emit(&format!("@@CLEAR_TEMPS:{}@@", fmark));
            let fexc = self.hidden("fexc");
            self.emit(&format!("{} = PyErr_GetRaisedException();", fexc));
            self.block(finalbody)?;
            self.emit(&format!("PyErr_SetRaisedException({}); {} = NULL; goto *__err;", fexc, fexc));
            self.emit(&format!("{}: ;", lfend));
        }
        self.fr().indent -= 1;
        self.emit("}");
        Ok(())
    }

    fn with_stmt(&mut self, items: &[WithItem], body: &[Stmt], is_async: bool) -> R<()> {
        if items.len() > 1 {
            let inner = Stmt { kind: StmtKind::With { items: items[1..].to_vec(), body: body.to_vec(), is_async }, span: Default::default(), end_line: 0, header_end: 0 };
            return self.with_stmt(&items[..1], &[inner], is_async);
        }
        // `async with` awaits the results of __aenter__/__aexit__
        let aw = |call: &str| if is_async { format!("fpc_await_steal(__gen, {})", call) } else { call.to_string() };
        let it = &items[0];
        let m = self.expr(&it.ctx)?;
        let mgr = self.hidden("mgr");
        let exitf = self.hidden("exit");
        self.emit(&format!("{} = {}; {} = NULL;", mgr, m, m));
        let ek = self.name_const(if is_async { "__aexit__" } else { "__exit__" });
        let nk = self.name_const(if is_async { "__aenter__" } else { "__enter__" });
        self.emit(&format!("{} = PyObject_GetAttr((PyObject *)Py_TYPE({}), {});", exitf, mgr, ek));
        self.emit(&format!("if (!{}) goto *__err;", exitf));
        let entered = self.call_new(&aw(&format!("PyObject_CallMethodNoArgs({}, {})", mgr, nk)));
        match &it.var {
            Some(v) => self.assign(v, &entered)?,
            None => self.release(&entered),
        }
        let saved = format!("__saved_err{}", self.tmpn + 1);
        self.tmpn += 1;
        let lw = self.label("w");
        let lwend = self.label("wend");
        let normal_exit = format!(
            "{{ PyObject *_r = {c}; if (!_r) goto *__err; Py_DECREF(_r); }} Py_CLEAR({x}); Py_CLEAR({m});",
            c = aw(&format!("PyObject_CallFunctionObjArgs({x}, {m}, Py_None, Py_None, Py_None, NULL)", x = exitf, m = mgr)),
            x = exitf,
            m = mgr
        );
        self.emit(&format!("{{ void *{} = __err; __err = &&{};", saved, lw));
        self.fr().indent += 1;
        self.fr().fins.push(Fin::Raw(format!("__err = {}; {}", saved, normal_exit)));
        let wmark = self.frr().hidden.len();
        self.block(body)?;
        self.fr().fins.pop();
        self.emit(&format!("__err = {};", saved));
        self.emit(&normal_exit);
        self.emit(&format!("goto {};", lwend));
        self.emit(&format!("{}:", lw));
        self.emit(&format!("__err = {};", saved));
        self.emit(&format!("@@CLEAR_TEMPS:{}@@", wmark));
        let e = self.hidden("wexc");
        self.emit(&format!("{} = PyErr_GetRaisedException();", e));
        self.emit(&format!(
            "{{ PyObject *_r = {c}; Py_CLEAR({x}); Py_CLEAR({m});",
            c = aw(&format!("PyObject_CallFunctionObjArgs({x}, {m}, (PyObject *)Py_TYPE({e}), {e}, Py_None, NULL)", x = exitf, m = mgr, e = e)),
            x = exitf,
            m = mgr
        ));
        self.emit(&format!("  if (!_r) {{ Py_CLEAR({e}); goto *__err; }}", e = e));
        self.emit(&format!("  int _s = PyObject_IsTrue(_r); Py_DECREF(_r); if (_s < 0) {{ Py_CLEAR({e}); goto *__err; }}", e = e));
        self.emit(&format!("  if (!_s) {{ PyErr_SetRaisedException({e}); {e} = NULL; goto *__err; }} Py_CLEAR({e}); }}", e = e));
        self.emit(&format!("{}: ;", lwend));
        self.fr().indent -= 1;
        self.emit("}");
        Ok(())
    }

    // ------------------------------------------------------------------ functions and classes

    /// Compile a function definition; returns a temp holding the function object.
    fn make_function(&mut self, f: &FuncDef) -> R<String> {
        // decorators are evaluated before the function object is created
        let mut decos = Vec::new();
        for d in &f.decorators {
            decos.push(self.expr(d)?);
        }
        self.next_is_async = f.is_async;
        let fv = self.function_object_ret(&f.name, &f.params, &f.body, false, f.returns.as_ref())?;
        let mut cur = fv;
        for d in decos.iter().rev() {
            let r = self.call_new(&format!("PyObject_CallOneArg({}, {})", d, cur));
            self.release(&cur);
            self.release(d);
            cur = r;
        }
        Ok(cur)
    }

    fn function_object(&mut self, name: &str, params: &Params, body: &[Stmt], is_lambda: bool) -> R<String> {
        self.function_object_ret(name, params, body, is_lambda, None)
    }

    fn function_object_ret(&mut self, name: &str, params: &Params, body: &[Stmt], is_lambda: bool, returns: Option<&Expr>) -> R<String> {
        self.nfn += 1;
        let cname = format!("{}_f{}_{}", self.prefix, self.nfn, c_ident(name));
        let mut sc = scope::analyze_function(params, body);
        let in_class = self.frr().kind == Kind::Class;
        // zero-argument super() / __class__ in methods capture the class cell
        if in_class && uses_name(body, "super") || uses_name(body, "__class__") {
            sc.frees.insert("__class__".into());
        }
        // defaults (evaluated now, in the defining scope)
        let pos: Vec<&Arg> = params.all_positional().collect();
        let ndef = pos.iter().filter(|a| a.default.is_some()).count();
        let defaults = if ndef > 0 {
            let mut items = Vec::new();
            for a in pos.iter().filter(|a| a.default.is_some()) {
                items.push(self.expr(a.default.as_ref().unwrap())?);
            }
            let t = self.call_new(&format!("PyTuple_Pack({}, {})", items.len(), items.join(", ")));
            for i in items {
                self.release(&i);
            }
            t
        } else {
            "NULL".to_string()
        };
        let kwdefaults = if params.kwonly.iter().any(|a| a.default.is_some()) {
            let d = self.call_new("PyDict_New()");
            for a in params.kwonly.iter().filter(|a| a.default.is_some()) {
                let v = self.expr(a.default.as_ref().unwrap())?;
                self.check_int(&format!("PyDict_SetItemString({}, {}, {})", d, c_string(a.name.as_bytes()), v));
                self.release(&v);
            }
            d
        } else {
            "NULL".to_string()
        };
        // closure: cells of enclosing scopes for this function's free variables
        let frees: Vec<String> = sc.frees.iter().filter(|n| self.resolves_to_cell(n)).cloned().collect();
        let closure = if frees.is_empty() {
            "NULL".to_string()
        } else {
            let mut cells = Vec::new();
            for n in &frees {
                cells.push(self.cell_ref(n)?);
            }
            self.call_new(&format!("PyTuple_Pack({}, {})", cells.len(), cells.join(", ")))
        };
        // compile the body in a new frame
        let mut fr = Frame::new(Kind::Function);
        fr.free_names = frees.clone();
        for (i, n) in frees.iter().enumerate() {
            fr.locals.insert(n.clone(), LVar::Free(i));
        }
        let mut slot_names: Vec<String> = pos.iter().map(|a| a.name.clone()).collect();
        slot_names.extend(params.kwonly.iter().map(|a| a.name.clone()));
        if let Some(v) = &params.vararg {
            slot_names.push(v.name.clone());
        }
        if let Some(k) = &params.kwarg {
            slot_names.push(k.name.clone());
        }
        fr.first_arg = slot_names.first().cloned();
        let is_async = std::mem::take(&mut self.next_is_async);
        fr.is_coro = is_async;
        fr.is_gen = sc.has_yield || is_async;
        for n in sc.locals.iter() {
            if sc.globals.contains(n) {
                continue;
            }
            let cv = if sc.cells.contains(n) { LVar::Cell(format!("c_{}", c_ident(n))) } else { LVar::Fast(format!("l_{}", c_ident(n))) };
            fr.locals.insert(n.clone(), cv);
        }
        for g in &sc.globals {
            fr.globals_decl.insert(g.clone());
        }
        self.frames.push(fr);
        let qual = format!("{}{}", self.qual_prefix, name);
        let saved_qual = std::mem::replace(&mut self.qual_prefix, format!("{}.<locals>.", qual));
        let r = (|| -> R<()> {
            if is_lambda {
                if let Some(Stmt { kind: StmtKind::Return(Some(e)), .. }) = body.first() {
                    let t = self.expr(e)?;
                    self.emit(&format!("__ret = {}; {} = NULL; goto L_ret;", t, t));
                }
            } else {
                self.block(body)?;
            }
            Ok(())
        })();
        let fr = self.frames.pop().unwrap();
        self.qual_prefix = saved_qual;
        r?;
        // assemble the C function (generators: the body runs on a coroutine stack)
        let is_gen = fr.is_gen;
        let body_name = if is_gen { format!("{}_body", cname) } else { cname.clone() };
        let gen_param = if is_gen { ", FpyGen *__gen" } else { "" };
        let mut c = format!("static PyObject *{}(PyObject *__self, PyObject **__slots{}) {{\n    PyObject *__globals = fpc_globals(__self);\n    PyObject *__ret = NULL;\n    void *__err = &&L_err;\n    (void)__globals;\n", body_name, gen_param);
        let mut locals_decl = String::new();
        let mut cleanup = Vec::new();
        let mut owned: Vec<(String, bool)> = Vec::new();
        for (n, lv) in &fr.locals {
            match lv {
                LVar::Fast(cv) => owned.push((cv.clone(), slot_names.contains(n))),
                LVar::Cell(cv) => owned.push((cv.clone(), slot_names.contains(n))),
                LVar::Free(_) => {}
            }
        }
        owned.sort();
        for (cv, _) in &owned {
            locals_decl.push_str(&format!("    PyObject *{} = NULL;\n", cv));
            cleanup.push(format!("Py_CLEAR({});", cv));
        }
        c.push_str(&locals_decl);
        for (i, n) in slot_names.iter().enumerate() {
            match fr.locals.get(n) {
                Some(LVar::Fast(cv)) => c.push_str(&format!("    {} = __slots[{}];\n", cv, i)),
                Some(LVar::Cell(cv)) => c.push_str(&format!("    {} = PyCell_New(__slots[{}]); Py_DECREF(__slots[{}]);\n", cv, i, i)),
                _ => c.push_str(&format!("    Py_DECREF(__slots[{}]);\n", i)),
            }
        }
        for (n, lv) in &fr.locals {
            if let LVar::Cell(cv) = lv {
                if !slot_names.contains(n) {
                    c.push_str(&format!("    {} = PyCell_New(NULL);\n    if (!{}) goto L_err;\n", cv, cv));
                }
            }
        }
        for t in fr.temps.iter().chain(fr.hidden.iter()) {
            c.push_str(&format!("    PyObject *{} = NULL;\n", t));
            cleanup.push(format!("Py_CLEAR({});", t));
        }
        c.push_str(&expand_clears(&fr.body, &fr));
        c.push_str("    __ret = Py_NewRef(Py_None);\nL_ret:\n");
        c.push_str(&format!("    {}\n    return __ret;\nL_err:\n    {}\n    Py_CLEAR(__ret);\n    return NULL;\n}}\n\n", cleanup.join(" "), cleanup.join(" ")));
        if is_gen {
            self.protos.push_str(&format!("static PyObject *{}(PyObject *__self, PyObject **__slots, FpyGen *__gen);\n", body_name));
            c.push_str(&format!(
                "static PyObject *{}(PyObject *__self, PyObject **__slots) {{\n    return {}({}, __self, __slots, {}, {});\n}}\n\n",
                cname,
                if is_async { "fpc_make_coroutine" } else { "fpc_make_generator" },
                body_name,
                slot_names.len(),
                c_string(name.as_bytes())
            ));
        }
        // parameter spec
        let mut pnames: Vec<String> = slot_names.iter().map(|n| c_string(n.as_bytes())).collect();
        if pnames.is_empty() {
            pnames.push("NULL".into());
        }
        let spec = format!("{}_spec", cname);
        // annotation sources, in slot order (positional, kw-only, *args, **kwargs)
        let src = std::fs::read_to_string(&self.path).unwrap_or_default();
        let ann_src = |a: &Arg| -> String {
            match &a.ann {
                Some(x) => c_string(src.get(x.span.start..x.span.end).unwrap_or("").as_bytes()),
                None => "NULL".into(),
            }
        };
        let mut anns: Vec<String> = pos.iter().map(|a| ann_src(a)).collect();
        anns.extend(params.kwonly.iter().map(|a| ann_src(a)));
        if let Some(v) = &params.vararg {
            anns.push(ann_src(v));
        }
        if let Some(k) = &params.kwarg {
            anns.push(ann_src(k));
        }
        if anns.is_empty() {
            anns.push("NULL".into());
        }
        let ret_ann = match returns {
            Some(r) => c_string(src.get(r.span.start..r.span.end).unwrap_or("").as_bytes()),
            None => "NULL".to_string(),
        };
        self.protos.push_str(&format!("static PyObject *{}(PyObject *__self, PyObject **__slots);\n", cname));
        self.protos.push_str(&format!("static const char *const {}_annots[] = {{{}}};\n", cname, anns.join(", ")));
        self.protos.push_str(&format!(
            "static const char *const {}_params[] = {{{}}};\nstatic const FpcSpec {} = {{{}, {}, {}, {}, {}, {}, {}, {}, {}_params, {}_annots, {}, {}}};\n",
            cname,
            pnames.join(", "),
            spec,
            c_string(name.as_bytes()),
            c_string(qual.as_bytes()),
            cname,
            params.posonly.len(),
            pos.len(),
            params.kwonly.len(),
            params.vararg.is_some() as i32,
            params.kwarg.is_some() as i32,
            cname,
            cname,
            ret_ann,
            is_async as i32
        ));
        self.funcs.push_str(&c);
        let doc = match body.first().map(|s| &s.kind) {
            Some(StmtKind::Expr(Expr { kind: ExprKind::Str { value, .. }, .. })) if !is_lambda => self.str_const(value),
            _ => "NULL".into(),
        };
        let fobj = self.call_new(&format!("fpc_make_function(&{}, __globals, {}, {}, {}, {})", spec, defaults, kwdefaults, closure, doc));
        for t in [defaults, kwdefaults, closure] {
            if t != "NULL" {
                self.release(&t);
            }
        }
        Ok(fobj)
    }

    fn resolves_to_cell(&self, n: &str) -> bool {
        if n == "__class__" {
            return self.frr().class_cell.is_some() || matches!(self.frr().locals.get(n), Some(LVar::Free(_)));
        }
        // methods of a class defined inside a function close over that function's cells
        matches!(self.frr().locals.get(n), Some(LVar::Cell(_)) | Some(LVar::Free(_))) && (self.frr().kind == Kind::Function || self.frr().class_in_fn)
    }

    /// Borrowed reference to the cell object holding `n` in the current frame.
    fn cell_ref(&mut self, n: &str) -> R<String> {
        if n == "__class__" {
            if let Some(cc) = self.frr().class_cell.clone() {
                return Ok(cc);
            }
        }
        match self.frr().locals.get(n).cloned() {
            Some(LVar::Cell(c)) => Ok(c),
            Some(LVar::Free(i)) => Ok(format!("fpc_closure_cell(__self, {})", i)),
            _ => Err(format!("cannot capture '{}'", n)),
        }
    }

    fn class_def(&mut self, c: &ClassDef) -> R<()> {
        let mut decos = Vec::new();
        for d in &c.decorators {
            decos.push(self.expr(d)?);
        }
        let mut bases = Vec::new();
        for b in &c.bases {
            if let ExprKind::Starred(_) = b.kind {
                return Err("starred class bases".into());
            }
            bases.push(self.expr(b)?);
        }
        let bt = self.call_new(&format!("PyTuple_Pack({}{})", bases.len(), bases.iter().map(|b| format!(", {}", b)).collect::<String>()));
        for b in &bases {
            self.release(b);
        }
        let kw = self.call_new("PyDict_New()");
        for k in &c.keywords {
            let name = k.name.clone().ok_or("class keyword splat")?;
            let v = self.expr(&k.value)?;
            self.check_int(&format!("PyDict_SetItemString({}, {}, {})", kw, c_string(name.as_bytes()), v));
            self.release(&v);
        }
        let nm = self.str_const(&c.name);
        let cqual = format!("{}{}", self.qual_prefix, c.name);
        let qv = self.str_const(&cqual);
        let ns = self.hidden("ns");
        self.emit(&format!("{} = fpc_prepare({}, {}, {});", ns, nm, bt, kw));
        self.emit(&format!("if (!{}) goto *__err;", ns));
        let modk = self.name_const("__module__");
        let qualk = self.name_const("__qualname__");
        let nmk = self.name_const("__name__");
        let modv = self.call_new(&format!("fpc_load_global(__globals, {})", nmk));
        self.check_int(&format!("PyObject_SetItem({}, {}, {})", ns, modk, modv));
        self.release(&modv);
        self.check_int(&format!("PyObject_SetItem({}, {}, {})", ns, qualk, qv));
        // class cell for zero-argument super()
        let cell = self.hidden("ccell");
        self.emit(&format!("{} = PyCell_New(NULL); if (!{}) goto *__err;", cell, cell));
        // body executes with the namespace as its local scope
        let saved_kind = self.frr().kind;
        let saved_ns = self.frr().ns.clone();
        let saved_cell = self.frr().class_cell.clone();
        let saved_locals = self.frr().locals.clone();
        let saved_in_fn = self.frr().class_in_fn;
        self.fr().class_in_fn = saved_in_fn || saved_kind == Kind::Function;
        self.fr().kind = Kind::Class;
        self.fr().ns = Some(ns.clone());
        self.fr().class_cell = Some(cell.clone());
        let saved_qual = std::mem::replace(&mut self.qual_prefix, format!("{}.", cqual));
        // functions defined in the class body cannot see class-level names
        let r = self.block(&c.body);
        self.qual_prefix = saved_qual;
        self.fr().kind = saved_kind;
        self.fr().ns = saved_ns;
        self.fr().class_cell = saved_cell;
        self.fr().locals = saved_locals;
        self.fr().class_in_fn = saved_in_fn;
        r?;
        let cls = self.call_new(&format!("fpc_build_class({}, {}, {}, {})", nm, bt, ns, kw));
        self.emit(&format!("PyCell_Set({}, {}); Py_CLEAR({}); Py_CLEAR({});", cell, cls, cell, ns));
        self.release(&bt);
        self.release(&kw);
        let mut cur = cls;
        for d in decos.iter().rev() {
            let r = self.call_new(&format!("PyObject_CallOneArg({}, {})", d, cur));
            self.release(&cur);
            self.release(d);
            cur = r;
        }
        self.store_name(&c.name, &cur)
    }

    // ------------------------------------------------------------------ expressions

    /// Evaluate a condition to a C int variable name.
    fn truth(&mut self, e: &Expr) -> R<String> {
        let t = self.expr(e)?;
        let v = self.int_var("c");
        self.emit(&format!("{} = PyObject_IsTrue({}); Py_CLEAR({});", v, t, t));
        self.emit(&format!("if ({} < 0) goto *__err;", v));
        Ok(v)
    }

    fn index_expr(&mut self, index: &Expr) -> R<String> {
        match &index.kind {
            ExprKind::Slice { lower, upper, step } => {
                let mut parts = Vec::new();
                for p in [lower, upper, step] {
                    parts.push(match p {
                        Some(x) => self.expr(x)?,
                        None => {
                            let t = self.temp();
                            self.emit(&format!("{} = Py_NewRef(Py_None);", t));
                            t
                        }
                    });
                }
                let s = self.call_new(&format!("PySlice_New({}, {}, {})", parts[0], parts[1], parts[2]));
                for p in parts {
                    self.release(&p);
                }
                Ok(s)
            }
            ExprKind::Tuple { elts, .. } => {
                let mut items = Vec::new();
                for e in elts {
                    items.push(self.index_expr(e)?);
                }
                self.pack_tuple(items)
            }
            _ => self.expr(index),
        }
    }

    fn pack_tuple(&mut self, items: Vec<String>) -> R<String> {
        let t = self.call_new(&format!("PyTuple_New({})", items.len()));
        for (i, x) in items.iter().enumerate() {
            self.emit(&format!("PyTuple_SET_ITEM({}, {}, {}); {} = NULL;", t, i, x, x));
        }
        Ok(t)
    }

    fn expr(&mut self, e: &Expr) -> R<String> {
        match &e.kind {
            ExprKind::Int(v) => {
                let k = self.konst(format!("i:{}", v), format!("PyLong_FromLongLong({}LL)", v));
                let t = self.temp();
                self.emit(&format!("{} = Py_NewRef({});", t, k));
                Ok(t)
            }
            ExprKind::BigInt(s) => {
                let k = self.konst(format!("b:{}", s), format!("PyLong_FromString({}, NULL, 0)", c_string(s.as_bytes())));
                let t = self.temp();
                self.emit(&format!("{} = Py_NewRef({});", t, k));
                Ok(t)
            }
            ExprKind::Float(v) => {
                let k = self.konst(format!("f:{:?}", v), format!("PyFloat_FromDouble({})", crate::compiler::float_lit_pub(*v)));
                let t = self.temp();
                self.emit(&format!("{} = Py_NewRef({});", t, k));
                Ok(t)
            }
            ExprKind::Imag(v) => {
                let k = self.konst(format!("j:{:?}", v), format!("PyComplex_FromDoubles(0.0, {})", crate::compiler::float_lit_pub(*v)));
                let t = self.temp();
                self.emit(&format!("{} = Py_NewRef({});", t, k));
                Ok(t)
            }
            ExprKind::Str { value, .. } => {
                let k = self.str_const(value);
                let t = self.temp();
                self.emit(&format!("{} = Py_NewRef({});", t, k));
                Ok(t)
            }
            ExprKind::Bytes { value, .. } => {
                let k = self.konst(format!("y:{:?}", value), format!("PyBytes_FromStringAndSize({}, {})", c_string(value), value.len()));
                let t = self.temp();
                self.emit(&format!("{} = Py_NewRef({});", t, k));
                Ok(t)
            }
            ExprKind::Bool(b) => {
                let t = self.temp();
                self.emit(&format!("{} = Py_NewRef({});", t, if *b { "Py_True" } else { "Py_False" }));
                Ok(t)
            }
            ExprKind::NoneLit => {
                let t = self.temp();
                self.emit(&format!("{} = Py_NewRef(Py_None);", t));
                Ok(t)
            }
            ExprKind::Ellipsis => {
                let t = self.temp();
                self.emit(&format!("{} = Py_NewRef(Py_Ellipsis);", t));
                Ok(t)
            }
            ExprKind::FStr { parts, .. } => self.fstring(parts),
            ExprKind::Name(n) => self.load_name(n),
            ExprKind::BinOp { left, op, right } => {
                let a = self.expr(left)?;
                let b = self.expr(right)?;
                let code = if *op == BinOp::Pow { format!("PyNumber_Power({}, {}, Py_None)", a, b) } else { format!("{}({}, {})", binop_fn(*op), a, b) };
                let r = self.call_new(&code);
                self.release(&a);
                self.release(&b);
                Ok(r)
            }
            ExprKind::UnaryOp { op, operand } => {
                let v = self.expr(operand)?;
                let r = match op {
                    UnOp::Not => {
                        let c = self.int_var("n");
                        self.emit(&format!("{} = PyObject_Not({});", c, v));
                        self.emit(&format!("if ({} < 0) goto *__err;", c));
                        let t = self.temp();
                        self.emit(&format!("{} = Py_NewRef({} ? Py_True : Py_False);", t, c));
                        t
                    }
                    UnOp::Neg => self.call_new(&format!("PyNumber_Negative({})", v)),
                    UnOp::Pos => self.call_new(&format!("PyNumber_Positive({})", v)),
                    UnOp::Invert => self.call_new(&format!("PyNumber_Invert({})", v)),
                };
                self.release(&v);
                Ok(r)
            }
            ExprKind::BoolOp { op, values } => {
                // result is the first operand deciding the outcome (Python semantics)
                let res = self.temp();
                let end = self.label("bo");
                for (i, v) in values.iter().enumerate() {
                    let t = self.expr(v)?;
                    self.emit(&format!("Py_XSETREF({}, {}); {} = NULL;", res, t, t));
                    if i + 1 < values.len() {
                        let c = self.int_var("b");
                        self.emit(&format!("{} = PyObject_IsTrue({});", c, res));
                        self.emit(&format!("if ({} < 0) goto *__err;", c));
                        match op {
                            BoolOp::And => self.emit(&format!("if (!{}) goto {};", c, end)),
                            BoolOp::Or => self.emit(&format!("if ({}) goto {};", c, end)),
                        }
                    }
                }
                self.emit(&format!("{}: ;", end));
                Ok(res)
            }
            ExprKind::Compare { left, ops, comparators } => {
                let res = self.temp();
                let end = self.label("cmp");
                let mut l = self.expr(left)?;
                for (i, (op, c)) in ops.iter().zip(comparators).enumerate() {
                    let r = self.expr(c)?;
                    let v = match op {
                        CmpOp::Is | CmpOp::IsNot => {
                            let t = self.temp();
                            let eq = if *op == CmpOp::Is { "==" } else { "!=" };
                            self.emit(&format!("{} = Py_NewRef(({} {} {}) ? Py_True : Py_False);", t, l, eq, r));
                            t
                        }
                        CmpOp::In | CmpOp::NotIn => {
                            let ci = self.int_var("in");
                            self.emit(&format!("{} = PySequence_Contains({}, {});", ci, r, l));
                            self.emit(&format!("if ({} < 0) goto *__err;", ci));
                            let t = self.temp();
                            let pos = if *op == CmpOp::In { "" } else { "!" };
                            self.emit(&format!("{} = Py_NewRef(({}{}) ? Py_True : Py_False);", t, pos, ci));
                            t
                        }
                        _ => self.call_new(&format!("PyObject_RichCompare({}, {}, {})", l, r, cmp_op(*op))),
                    };
                    self.emit(&format!("Py_XSETREF({}, {}); {} = NULL;", res, v, v));
                    self.release(&l);
                    l = r;
                    if i + 1 < ops.len() {
                        let c = self.int_var("cc");
                        self.emit(&format!("{} = PyObject_IsTrue({});", c, res));
                        self.emit(&format!("if ({} < 0) goto *__err;", c));
                        self.emit(&format!("if (!{}) goto {};", c, end));
                    }
                }
                self.emit(&format!("{}: ;", end));
                self.release(&l);
                Ok(res)
            }
            ExprKind::Call { func, args, keywords } => self.call(func, args, keywords),
            ExprKind::Attribute { value, attr } => {
                let o = self.expr(value)?;
                let nk = self.name_const(attr);
                let r = self.call_new(&format!("PyObject_GetAttr({}, {})", o, nk));
                self.release(&o);
                Ok(r)
            }
            ExprKind::Subscript { value, index } => {
                let o = self.expr(value)?;
                let k = self.index_expr(index)?;
                let r = self.call_new(&format!("PyObject_GetItem({}, {})", o, k));
                self.release(&o);
                self.release(&k);
                Ok(r)
            }
            ExprKind::List(elts) => self.collection("PyList_New(0)", "PyList_Append", elts, true),
            ExprKind::Set(elts) => self.collection("PySet_New(NULL)", "PySet_Add", elts, false),
            ExprKind::Tuple { elts, .. } => {
                if elts.iter().any(|e| matches!(e.kind, ExprKind::Starred(_))) {
                    let l = self.collection("PyList_New(0)", "PyList_Append", elts, true)?;
                    let t = self.call_new(&format!("PyList_AsTuple({})", l));
                    self.release(&l);
                    return Ok(t);
                }
                let mut items = Vec::new();
                for x in elts {
                    items.push(self.expr(x)?);
                }
                self.pack_tuple(items)
            }
            ExprKind::Dict(items) => {
                let d = self.call_new("PyDict_New()");
                for (k, v) in items {
                    match k {
                        Some(k) => {
                            let kk = self.expr(k)?;
                            let vv = self.expr(v)?;
                            self.check_int(&format!("PyDict_SetItem({}, {}, {})", d, kk, vv));
                            self.release(&kk);
                            self.release(&vv);
                        }
                        None => {
                            let vv = self.expr(v)?;
                            self.check_int(&format!("PyDict_Update({}, {})", d, vv));
                            self.release(&vv);
                        }
                    }
                }
                Ok(d)
            }
            ExprKind::ListComp { elt, gens } => self.comprehension(CompK::List, elt, None, gens),
            ExprKind::SetComp { elt, gens } => self.comprehension(CompK::Set, elt, None, gens),
            ExprKind::DictComp { key, value, gens } => self.comprehension(CompK::Dict, key, Some(value), gens),
            ExprKind::GenExp { elt, gens } => self.genexp(elt, gens, e.span),
            ExprKind::IfExp { test, body, orelse } => {
                let res = self.temp();
                let c = self.truth(test)?;
                self.emit(&format!("if ({}) {{", c));
                self.fr().indent += 1;
                let a = self.expr(body)?;
                self.emit(&format!("{} = {}; {} = NULL;", res, a, a));
                self.fr().indent -= 1;
                self.emit("} else {");
                self.fr().indent += 1;
                let b = self.expr(orelse)?;
                self.emit(&format!("{} = {}; {} = NULL;", res, b, b));
                self.fr().indent -= 1;
                self.emit("}");
                Ok(res)
            }
            ExprKind::Lambda { params, body } => {
                let st = Stmt { kind: StmtKind::Return(Some((**body).clone())), span: e.span, end_line: e.span.line, header_end: e.span.line };
                self.function_object("<lambda>", params, &[st], true)
            }
            ExprKind::NamedExpr { target, value } => {
                let v = self.expr(value)?;
                let dup = self.temp();
                self.emit(&format!("{} = Py_NewRef({});", dup, v));
                // walrus inside a comprehension binds in the enclosing function scope
                let saved: Vec<HashMap<String, String>> = std::mem::take(&mut self.fr().comp);
                let r = self.store_name(target, &dup);
                self.fr().comp = saved;
                r?;
                Ok(v)
            }
            ExprKind::Starred(_) => Err("starred expression in this position".into()),
            ExprKind::Slice { .. } => Err("slice outside subscript".into()),
            ExprKind::Yield(v) => {
                if !self.frr().is_gen {
                    return Err("yield outside a generator function".into());
                }
                let val = match v {
                    Some(x) => self.expr(x)?,
                    None => {
                        let t = self.temp();
                        self.emit(&format!("{} = Py_NewRef(Py_None);", t));
                        t
                    }
                };
                let r = self.temp();
                self.emit(&format!("{} = fpc_yield(__gen, {}); {} = NULL;", r, val, val));
                self.emit(&format!("if (!{}) goto *__err;", r));
                Ok(r)
            }
            ExprKind::YieldFrom(v) => {
                if !self.frr().is_gen {
                    return Err("yield from outside a generator function".into());
                }
                let it = self.expr(v)?;
                let r = self.call_new(&format!("fpc_yield_from(__gen, {})", it));
                self.release(&it);
                Ok(r)
            }
            ExprKind::Await(x) => {
                if !self.frr().is_coro {
                    return Err("'await' outside async function".into());
                }
                let v = self.expr(x)?;
                let r = self.call_new(&format!("fpc_await(__gen, {})", v));
                self.release(&v);
                Ok(r)
            }
        }
    }

    fn collection(&mut self, new: &str, add: &str, elts: &[Expr], is_list: bool) -> R<String> {
        let c = self.call_new(new);
        for x in elts {
            match &x.kind {
                ExprKind::Starred(inner) => {
                    let v = self.expr(inner)?;
                    if is_list {
                        self.check_int(&format!("PyList_Extend({}, {})", c, v));
                    } else {
                        let r = self.call_new(&format!("PyObject_CallMethod({}, \"update\", \"(O)\", {})", c, v));
                        self.release(&r);
                    }
                    self.release(&v);
                }
                _ => {
                    let v = self.expr(x)?;
                    self.check_int(&format!("{}({}, {})", add, c, v));
                    self.release(&v);
                }
            }
        }
        Ok(c)
    }

    fn call(&mut self, func: &Expr, args: &[Expr], keywords: &[Keyword]) -> R<String> {
        // super() with no arguments inside a method
        if let ExprKind::Name(n) = &func.kind {
            if n == "super" && args.is_empty() && keywords.is_empty() {
                if let Some(first) = self.frr().first_arg.clone() {
                    if self.frr().kind == Kind::Function && self.frr().locals.contains_key("__class__") {
                        let cls = self.load_name("__class__")?;
                        let me = self.load_name(&first)?;
                        let r = self.call_new(&format!("PyObject_CallFunctionObjArgs((PyObject *)&PySuper_Type, {}, {}, NULL)", cls, me));
                        self.release(&cls);
                        self.release(&me);
                        return Ok(r);
                    }
                }
            }
            if (n == "globals" || n == "locals" || n == "vars") && args.is_empty() && self.frr().kind != Kind::Function {
                let t = self.temp();
                let src = match (n.as_str(), self.frr().kind) {
                    ("globals", _) | (_, Kind::Module) => "__globals".to_string(),
                    _ => self.frr().ns.clone().unwrap(),
                };
                self.emit(&format!("{} = Py_NewRef({});", t, src));
                return Ok(t);
            }
            if n == "globals" && args.is_empty() {
                let t = self.temp();
                self.emit(&format!("{} = Py_NewRef(__globals);", t));
                return Ok(t);
            }
        }
        // f(genexp) for eager consumers: materialize as a list
        let mut args_v: Vec<Expr> = args.to_vec();
        if let ExprKind::Name(n) = &func.kind {
            if EAGER_CONSUMERS.contains(&n.as_str()) {
                for a in args_v.iter_mut() {
                    if let ExprKind::GenExp { elt, gens } = &a.kind {
                        *a = Expr::new(ExprKind::ListComp { elt: elt.clone(), gens: gens.clone() }, a.span);
                    }
                }
            }
        }
        if let ExprKind::Attribute { attr, .. } = &func.kind {
            if attr == "join" {
                for a in args_v.iter_mut() {
                    if let ExprKind::GenExp { elt, gens } = &a.kind {
                        *a = Expr::new(ExprKind::ListComp { elt: elt.clone(), gens: gens.clone() }, a.span);
                    }
                }
            }
        }
        let f = self.expr(func)?;
        let has_star = args_v.iter().any(|a| matches!(a.kind, ExprKind::Starred(_))) || keywords.iter().any(|k| k.name.is_none());
        if has_star {
            let l = self.collection("PyList_New(0)", "PyList_Append", &args_v, true)?;
            let kw = if keywords.is_empty() {
                "NULL".to_string()
            } else {
                let d = self.call_new("PyDict_New()");
                for k in keywords {
                    let v = self.expr(&k.value)?;
                    match &k.name {
                        Some(n) => self.check_int(&format!("PyDict_SetItemString({}, {}, {})", d, c_string(n.as_bytes()), v)),
                        None => self.check_int(&format!("PyDict_Update({}, {})", d, v)),
                    }
                    self.release(&v);
                }
                d
            };
            let r = self.call_new(&format!("fpc_call_star({}, {}, {})", f, l, kw));
            self.release(&f);
            self.release(&l);
            if kw != "NULL" {
                self.release(&kw);
            }
            return Ok(r);
        }
        let mut items = Vec::new();
        for a in &args_v {
            items.push(self.expr(a)?);
        }
        let mut kwnames = Vec::new();
        for k in keywords {
            items.push(self.expr(&k.value)?);
            kwnames.push(k.name.clone().unwrap());
        }
        let argv = if items.is_empty() { "NULL".to_string() } else { format!("(PyObject *[]){{{}}}", items.join(", ")) };
        let kwt = if kwnames.is_empty() {
            "NULL".to_string()
        } else {
            let names: Vec<String> = kwnames.iter().map(|n| self.str_const(n)).collect();
            let key = format!("kw:{}", kwnames.join(","));
            self.konst(key, format!("PyTuple_Pack({}, {})", names.len(), names.join(", ")))
        };
        let r = self.call_new(&format!("PyObject_Vectorcall({}, {}, {}, {})", f, argv, args_v.len(), kwt));
        self.release(&f);
        for i in items {
            self.release(&i);
        }
        Ok(r)
    }

    /// `(elt for t in it ...)` == a nested generator function called with `iter(it)`.
    fn genexp(&mut self, elt: &Expr, gens: &[Comprehension], span: crate::diag::Span) -> R<String> {
        let pname = ".0".to_string();
        let mk = |kind: StmtKind| Stmt { kind, span, end_line: span.line, header_end: span.line };
        let mut body = mk(StmtKind::Expr(Expr::new(ExprKind::Yield(Some(Box::new(elt.clone()))), span)));
        for (i, g) in gens.iter().enumerate().rev() {
            for c in g.ifs.iter().rev() {
                body = mk(StmtKind::If { test: c.clone(), body: vec![body], orelse: vec![] });
            }
            let iter = if i == 0 { Expr::new(ExprKind::Name(pname.clone()), span) } else { g.iter.clone() };
            body = mk(StmtKind::For { target: g.target.clone(), iter, body: vec![body], orelse: vec![], is_async: false });
        }
        let params = Params { args: vec![Arg { name: pname, ann: None, default: None, span }], ..Default::default() };
        let first = self.expr(&gens[0].iter)?;
        let it = self.call_new(&format!("PyObject_GetIter({})", first));
        self.release(&first);
        let f = self.function_object("<genexpr>", &params, &[body], false)?;
        let r = self.call_new(&format!("PyObject_CallOneArg({}, {})", f, it));
        self.release(&f);
        self.release(&it);
        Ok(r)
    }

    fn fstring(&mut self, parts: &[FStrPart]) -> R<String> {
        let mut items = Vec::new();
        for p in parts {
            match p {
                FStrPart::Lit(s) => {
                    let k = self.str_const(s);
                    let t = self.temp();
                    self.emit(&format!("{} = Py_NewRef({});", t, k));
                    items.push(t);
                }
                FStrPart::Expr { expr, conv, spec, debug_text } => {
                    if let Some(d) = debug_text {
                        let k = self.str_const(d);
                        let t = self.temp();
                        self.emit(&format!("{} = Py_NewRef({});", t, k));
                        items.push(t);
                    }
                    let v = self.expr(expr)?;
                    let sp = if spec.is_empty() {
                        let k = self.str_const("");
                        let t = self.temp();
                        self.emit(&format!("{} = Py_NewRef({});", t, k));
                        t
                    } else {
                        self.fstring(spec)?
                    };
                    let conv_c = match conv {
                        Some(c) => *c as u32,
                        None if debug_text.is_some() && spec.is_empty() => 'r' as u32,
                        None => 0,
                    };
                    let r = self.call_new(&format!("fpc_format_value({}, {}, {})", v, conv_c, sp));
                    self.release(&v);
                    self.release(&sp);
                    items.push(r);
                }
            }
        }
        let t = self.pack_tuple(items)?;
        let empty = self.str_const("");
        let r = self.call_new(&format!("PyUnicode_Join({}, {})", empty, t));
        self.release(&t);
        Ok(r)
    }

    fn comprehension(&mut self, kind: CompK, elt: &Expr, value: Option<&Expr>, gens: &[Comprehension]) -> R<String> {
        let res = match kind {
            CompK::List => self.call_new("PyList_New(0)"),
            CompK::Set => self.call_new("PySet_New(NULL)"),
            CompK::Dict => self.call_new("PyDict_New()"),
        };
        // comprehension variables live in hidden slots
        let mut names = BTreeSet::new();
        for g in gens {
            scope::collect_target_names(&g.target, &mut names);
        }
        let mut map = HashMap::new();
        for n in names {
            let h = self.hidden(&format!("cv_{}_", c_ident(&n)));
            map.insert(n, h);
        }
        // the first iterable is evaluated in the enclosing scope
        let first = self.expr(&gens[0].iter)?;
        self.fr().comp.push(map.clone());
        let r = self.comp_level(kind, elt, value, gens, 0, &res, Some(first));
        self.fr().comp.pop();
        r?;
        for h in map.values() {
            self.emit(&format!("Py_CLEAR({});", h));
        }
        Ok(res)
    }

    fn comp_level(&mut self, kind: CompK, elt: &Expr, value: Option<&Expr>, gens: &[Comprehension], i: usize, res: &str, first: Option<String>) -> R<()> {
        if i == gens.len() {
            let v = self.expr(elt)?;
            match kind {
                CompK::List => self.check_int(&format!("PyList_Append({}, {})", res, v)),
                CompK::Set => self.check_int(&format!("PySet_Add({}, {})", res, v)),
                CompK::Dict => {
                    let vv = self.expr(value.unwrap())?;
                    self.check_int(&format!("PyDict_SetItem({}, {}, {})", res, v, vv));
                    self.release(&vv);
                }
            }
            self.release(&v);
            return Ok(());
        }
        let g = &gens[i];
        let x = match first {
            Some(f) => f,
            None => self.expr(&g.iter)?,
        };
        let it = self.hidden("cit");
        self.emit(&format!("{} = PyObject_GetIter({}); Py_CLEAR({});", it, x, x));
        self.emit(&format!("if (!{}) goto *__err;", it));
        self.emit("for (;;) {");
        self.fr().indent += 1;
        let item = self.temp();
        self.emit(&format!("{} = PyIter_Next({});", item, it));
        self.emit(&format!("if (!{}) {{ if (PyErr_Occurred()) goto *__err; break; }}", item));
        self.assign(&g.target, &item)?;
        for c in &g.ifs {
            let ct = self.truth(c)?;
            self.emit(&format!("if (!{}) continue;", ct));
        }
        self.comp_level(kind, elt, value, gens, i + 1, res, None)?;
        self.fr().indent -= 1;
        self.emit("}");
        self.emit(&format!("Py_CLEAR({});", it));
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum CompK {
    List,
    Set,
    Dict,
}

fn check_expr(e: &Expr, err: &mut Option<String>, in_eager_call: bool) {
    if err.is_some() {
        return;
    }
    let comp_gens = match &e.kind {
        ExprKind::ListComp { gens, .. } | ExprKind::SetComp { gens, .. } | ExprKind::DictComp { gens, .. } | ExprKind::GenExp { gens, .. } => Some(gens),
        _ => None,
    };
    if comp_gens.map(|gs| gs.iter().any(|g| g.is_async)).unwrap_or(false) {
        *err = Some("async comprehension".into());
        return;
    }
    match &e.kind {
        ExprKind::GenExp { elt, gens } => {
            let _ = in_eager_call;
            // a generator expression containing `await` is an async generator
            let mut has_await = false;
            fn find_await(e: &Expr, found: &mut bool) {
                if matches!(e.kind, ExprKind::Await(_)) {
                    *found = true;
                }
                scope::walk_children(e, &mut |c| find_await(c, found));
            }
            find_await(elt, &mut has_await);
            gens.iter().flat_map(|g| g.ifs.iter()).for_each(|c| find_await(c, &mut has_await));
            if has_await {
                *err = Some("await inside a generator expression (async generator)".into());
                return;
            }
            check_expr(elt, err, false);
            for g in gens {
                check_expr(&g.iter, err, false);
                for c in &g.ifs {
                    check_expr(c, err, false);
                }
            }
        }
        ExprKind::Call { func, args, keywords } => {
            check_expr(func, err, false);
            let eager = match &func.kind {
                ExprKind::Name(n) => EAGER_CONSUMERS.contains(&n.as_str()),
                ExprKind::Attribute { attr, .. } => attr == "join",
                _ => false,
            };
            for a in args {
                check_expr(a, err, eager);
            }
            for k in keywords {
                check_expr(&k.value, err, false);
            }
        }
        ExprKind::Name(n) if matches!(n.as_str(), "locals" | "vars" | "exec" | "eval") => *err = Some(format!("frame-dependent builtin '{}'", n)),
        ExprKind::Lambda { body, .. } => {
            if matches!(body.kind, ExprKind::Yield(_) | ExprKind::YieldFrom(_)) {
                *err = Some("yield inside lambda".into());
                return;
            }
            check_expr(body, err, false)
        }
        _ => scope::walk_children(e, &mut |c| check_expr(c, err, false)),
    }
}

fn uses_name(body: &[Stmt], name: &str) -> bool {
    let mut found = false;
    fn ex(e: &Expr, name: &str, found: &mut bool) {
        if let ExprKind::Name(n) = &e.kind {
            if n == name {
                *found = true;
            }
        }
        scope::walk_children(e, &mut |c| ex(c, name, found));
    }
    fn st(s: &Stmt, name: &str, found: &mut bool) {
        if let StmtKind::FuncDef(_) | StmtKind::ClassDef(_) = &s.kind {
            return;
        }
        let mut nodes = Vec::new();
        scope::walk_stmt(s, &mut |n| nodes.push(n));
        for n in nodes {
            match n {
                scope::Node::E(e) => ex(e, name, found),
                scope::Node::B(b) => b.iter().for_each(|s| st(s, name, found)),
            }
        }
    }
    body.iter().for_each(|s| st(s, name, &mut found));
    found
}

fn binop_fn(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "PyNumber_Add",
        BinOp::Sub => "PyNumber_Subtract",
        BinOp::Mul => "PyNumber_Multiply",
        BinOp::Div => "PyNumber_TrueDivide",
        BinOp::FloorDiv => "PyNumber_FloorDivide",
        BinOp::Mod => "PyNumber_Remainder",
        BinOp::Pow => "PyNumber_Power",
        BinOp::MatMul => "PyNumber_MatrixMultiply",
        BinOp::LShift => "PyNumber_Lshift",
        BinOp::RShift => "PyNumber_Rshift",
        BinOp::BitAnd => "PyNumber_And",
        BinOp::BitOr => "PyNumber_Or",
        BinOp::BitXor => "PyNumber_Xor",
    }
}

fn inplace_fn(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "PyNumber_InPlaceAdd",
        BinOp::Sub => "PyNumber_InPlaceSubtract",
        BinOp::Mul => "PyNumber_InPlaceMultiply",
        BinOp::Div => "PyNumber_InPlaceTrueDivide",
        BinOp::FloorDiv => "PyNumber_InPlaceFloorDivide",
        BinOp::Mod => "PyNumber_InPlaceRemainder",
        BinOp::Pow => "fpc_inplace_pow",
        BinOp::MatMul => "PyNumber_InPlaceMatrixMultiply",
        BinOp::LShift => "PyNumber_InPlaceLshift",
        BinOp::RShift => "PyNumber_InPlaceRshift",
        BinOp::BitAnd => "PyNumber_InPlaceAnd",
        BinOp::BitOr => "PyNumber_InPlaceOr",
        BinOp::BitXor => "PyNumber_InPlaceXor",
    }
}

fn cmp_op(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "Py_EQ",
        CmpOp::NotEq => "Py_NE",
        CmpOp::Lt => "Py_LT",
        CmpOp::LtE => "Py_LE",
        CmpOp::Gt => "Py_GT",
        CmpOp::GtE => "Py_GE",
        _ => "Py_EQ",
    }
}

/// Generate the registry that installs the meta-path finder for all compiled modules.
pub fn registry(mods: &[CompiledModule]) -> String {
    let mut s = String::new();
    for m in mods {
        s.push_str(&format!("int {}(PyObject *__module);\n", m.exec_fn));
    }

    s.push_str("static const FpcModule fpc_compiled_modules[] = {\n");
    for m in mods {
        s.push_str(&format!("    {{{}, {}, {}}},\n", c_string(m.name.as_bytes()), m.exec_fn, m.is_package as i32));
    }
    if mods.is_empty() {
        s.push_str("    {NULL, NULL, 0},\n");
    }
    s.push_str("};\n");
    s.push_str(&format!("static int fpc_install_compiled(void) {{ return fpc_register_modules(fpc_compiled_modules, {}); }}\n", mods.len()));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_constructs_needing_the_interpreter() {
        let cases = [
            ("match x:\n    case 1:\n        pass\n", "match"),
            ("async def g():\n    yield 1\n", "async generator"),
            ("try:\n    pass\nexcept* ValueError:\n    pass\n", "except*"),
            ("def f():\n    return locals()\n", "frame-dependent"),
            ("def f():\n    return eval('1')\n", "frame-dependent"),
            ("f = lambda: (yield)\n", "lambda"),
            ("async def a(xs):\n    return [x async for x in xs]\n", "async comprehension"),
            ("async def b(f):\n    return list(await f(x) for x in range(3))\n", "async generator"),
            ("class C(*bases):\n    pass\n", "starred class bases"),
            ("def f():\n    await x\n", "await"),
        ];
        let mut bad = Vec::new();
        for (src, want) in cases {
            match compile_module("m", "m.py", src) {
                Ok(_) => bad.push(format!("{:?}: compiled", src)),
                Err(e) if !e.contains(want) => bad.push(format!("{:?}: {:?} lacks {:?}", src, e, want)),
                _ => {}
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
        assert!(compile_module("m", "m.py", "def (:\n").is_err());
    }

    #[test]
    fn compiles_ordinary_modules() {
        let m = compile_module("pkg.mod", "mod.py", "import os\n\n\nclass A:\n    def m(self):\n        return [x for x in range(3)]\n\n\nasync def co():\n    async with x as y:\n        pass\n").unwrap();
        assert!(m.c_code.contains("fpc_make_coroutine"));
        assert!(m.exec_fn.contains("pkg_mod"));
        assert!(!m.is_package);
    }
}
