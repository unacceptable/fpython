//! Function bodies and statements.

use super::scope::{self, Scope};
use super::*;

#[derive(Debug, Clone)]
pub struct Local {
    pub ty: Ty,
    pub cname: String,
    pub cell: bool,
}

#[derive(Debug, Clone)]
pub enum Cleanup {
    /// An active exception frame that must be popped when jumping out.
    Frame(String),
    /// A finally block to run (statements, compiled inline) when jumping out.
    Finally(Vec<Stmt>),
}

#[derive(Debug, Clone)]
pub struct LoopCtx {
    pub cleanup_depth: usize,
    pub broke: Option<String>,
}

pub struct Fx {
    /// signatures of nested `def`s in this function, so calls by name can use defaults,
    /// keywords and *args/**kwargs
    pub nested_sigs: HashMap<String, Sig>,
    pub module: usize,
    pub class: Option<ClassId>,
    pub scope: Rc<Scope>,
    pub locals: HashMap<String, Local>,
    pub decls: Vec<String>,
    pub body: String,
    pub indent: usize,
    pub ret: Ty,
    pub subst: HashMap<String, Ty>,
    /// For generator bodies: (C name of the FpyGen* variable, element type).
    pub gen: Option<(String, Ty)>,
    /// type of `yield` expressions: the send type S of `Generator[Y, S, R]` (None otherwise)
    pub gen_send: Ty,
    pub cleanup: Vec<Cleanup>,
    pub loops: Vec<LoopCtx>,
    pub narrow: Vec<(String, Ty)>,
    pub comp: Vec<HashMap<String, Local>>,
    /// Captured variables of a closure: name -> (env slot, type).
    pub env: HashMap<String, (usize, Ty)>,
    pub is_module: bool,
    pub dry_run: bool,
    pub handler_exc: Vec<String>,
    pub fname: String,
    /// For methods of Python-derived classes: global holding the class object.
    pub py_class: Option<usize>,
    /// Inside `async def`: the declared result type.
    pub async_ret: Option<Ty>,
}

impl Fx {
    pub fn new(module: usize, class: Option<ClassId>) -> Fx {
        Fx {
            nested_sigs: HashMap::new(),
            module,
            class,
            scope: Rc::new(Scope::default()),
            locals: HashMap::new(),
            decls: Vec::new(),
            body: String::new(),
            indent: 1,
            ret: Ty::NoneT,
            subst: HashMap::new(),
            gen: None,
            gen_send: Ty::NoneT,
            cleanup: Vec::new(),
            loops: Vec::new(),
            narrow: Vec::new(),
            comp: Vec::new(),
            env: HashMap::new(),
            is_module: false,
            dry_run: false,
            handler_exc: Vec::new(),
            fname: String::new(),
            py_class: None,
            async_ret: None,
        }
    }
}

/// A typed C expression.
#[derive(Debug, Clone)]
pub struct CE {
    pub ty: Ty,
    pub c: String,
}

impl CE {
    pub fn new(ty: Ty, c: impl Into<String>) -> CE {
        CE { ty, c: c.into() }
    }
}

pub fn always_terminates(body: &[Stmt]) -> bool {
    match body.last().map(|s| &s.kind) {
        Some(StmtKind::Return(_)) | Some(StmtKind::Raise { .. }) => true,
        Some(StmtKind::If { body, orelse, .. }) => always_terminates(body) && always_terminates(orelse),
        Some(StmtKind::While { test, body, .. }) => matches!(test.kind, ExprKind::Bool(true)) && !contains_break(body),
        Some(StmtKind::Try { body, handlers, orelse, finalbody, .. }) => {
            always_terminates(finalbody)
                || ((always_terminates(body) || always_terminates(orelse)) && handlers.iter().all(|h| always_terminates(&h.body)))
        }
        Some(StmtKind::With { body, .. }) => always_terminates(body),
        Some(StmtKind::Match { cases, .. }) => {
            cases.iter().any(|c| matches!(c.pattern, Pattern::Wildcard) && c.guard.is_none()) && cases.iter().all(|c| always_terminates(&c.body))
        }
        Some(StmtKind::Expr(e)) => is_noreturn_call(e),
        _ => false,
    }
}

fn is_noreturn_call(e: &Expr) -> bool {
    if let ExprKind::Call { func, .. } = &e.kind {
        return match &func.kind {
            ExprKind::Name(n) => n == "exit" || n == "quit",
            ExprKind::Attribute { value, attr } => attr == "exit" && matches!(&value.kind, ExprKind::Name(n) if n == "sys"),
            _ => false,
        };
    }
    false
}

fn contains_break(body: &[Stmt]) -> bool {
    body.iter().any(|s| match &s.kind {
        StmtKind::Break => true,
        StmtKind::If { body, orelse, .. } => contains_break(body) || contains_break(orelse),
        StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
            contains_break(body) || contains_break(orelse) || contains_break(finalbody) || handlers.iter().any(|h| contains_break(&h.body))
        }
        StmtKind::With { body, .. } => contains_break(body),
        StmtKind::Match { cases, .. } => cases.iter().any(|c| contains_break(&c.body)),
        _ => false,
    })
}

impl Compiler {
    pub fn fx(&mut self) -> &mut Fx {
        self.fx_stack.last_mut().unwrap()
    }
    pub fn fxr(&self) -> &Fx {
        self.fx_stack.last().unwrap()
    }
    pub fn cur_mod(&self) -> usize {
        self.fxr().module
    }
    pub fn e(&self, span: Span, msg: impl Into<String>) -> Diag {
        self.err(self.cur_mod(), span, msg)
    }

    pub fn emit(&mut self, line: &str) {
        let fx = self.fx();
        if fx.dry_run {
            return;
        }
        for _ in 0..fx.indent {
            fx.body.push_str("    ");
        }
        fx.body.push_str(line);
        fx.body.push('\n');
    }

    pub fn decl(&mut self, d: String) {
        self.fx().decls.push(d);
    }

    pub fn tmp_var(&mut self, ty: &Ty, prefix: &str) -> String {
        let n = self.fresh(prefix);
        let ct = self.ctype(ty);
        self.decl(format!("{} {} = {};", ct, n, ty.zero()));
        n
    }

    // ------------------------------------------------------------------ function instances

    pub fn compile_function(&mut self, job: Job) -> R<()> {
        let fid = job.func;
        let def = self.funcs[fid].def.clone();
        let m = self.funcs[fid].module;
        let owner = self.funcs[fid].owner;
        let mut sig = self.func_sig(fid)?;
        let mut subst = self.funcs[fid].subst.clone();
        for (tp, t) in def.type_params.iter().zip(job.targs.iter()) {
            subst.insert(tp.name.clone(), t.clone());
        }
        if !job.targs.is_empty() {
            for p in &mut sig.params {
                p.ty = p.ty.subst(&subst);
            }
            if let Some((_, t)) = &mut sig.vararg {
                *t = t.subst(&subst);
            }
            sig.ret = sig.ret.subst(&subst);
        }
        let scope = self.func_scope(fid);
        if scope.has_yield && !def.is_async && !matches!(sig.ret, Ty::Iter(_)) {
            return Err(self.err(m, Span { line: def.body[0].span.line, col: 1, start: 0, end: 0 }, format!("generator '{}' must return Iterator[T]", def.name)));
        }
        let mut fx = Fx::new(m, owner);
        fx.scope = scope.clone();
        fx.ret = sig.ret.clone();
        fx.subst = subst;
        fx.fname = self.funcs[fid].qualname.clone();
        fx.py_class = self.funcs[fid].py_class;
        fx.async_ret = self.funcs[fid].async_ret.clone();
        self.fx_stack.push(fx);
        let is_gen = scope.has_yield || def.is_async;
        let res = self.compile_function_body(&job.cname, &def, &sig, is_gen, None);
        self.fx_stack.pop();
        let (proto, code) = res?;
        self.out.protos.push_str(&proto);
        self.out.funcs.push_str(&code);
        Ok(())
    }

    /// Emits a function (or closure, when `env` is Some) and returns (prototype, definition).
    pub fn compile_function_body(&mut self, cname: &str, def: &FuncDef, sig: &Sig, is_gen: bool, env: Option<&[(String, Ty)]>) -> R<(String, String)> {
        let rett = if is_gen { Ty::NoneT } else { sig.ret.clone() };
        let mut cparams: Vec<String> = Vec::new();
        if env.is_some() {
            cparams.push("void *env_".into());
        }
        let mut all_params: Vec<(String, Ty)> = sig.params.iter().map(|p| (p.name.clone(), p.ty.clone())).collect();
        if let Some((n, t)) = &sig.vararg {
            let pos = sig.params.iter().position(|p| p.kwonly).unwrap_or(sig.params.len());
            all_params.insert(pos, (n.clone(), t.clone()));
        }
        if let Some((n, t)) = &sig.kwarg {
            all_params.push((n.clone(), t.clone()));
        }
        for (n, t) in &all_params {
            cparams.push(format!("{} p_{}", self.ctype(t), c_ident(n)));
        }
        let params_c = if cparams.is_empty() { "void".to_string() } else { cparams.join(", ") };

        if let Some(caps) = env {
            self.decl("FpyTuple *env = (FpyTuple*)env_;".into());
            for (i, (n, t)) in caps.iter().enumerate() {
                self.fx().env.insert(n.clone(), (i, t.clone()));
            }
        }
        // Parameters become locals (cells if captured).
        for (n, t) in &all_params {
            let cell = self.fxr().scope.cells.contains(n);
            let local = self.declare_local(n, t)?;
            let pc = format!("p_{}", c_ident(n));
            if cell {
                let w = t.to_word(&pc);
                self.emit(&format!("{}->v = {};", local.cname, w));
            } else {
                self.emit(&format!("{} = {};", local.cname, pc));
            }
        }
        if is_gen {
            let elem = match &sig.ret {
                Ty::Iter(e) => (**e).clone(),
                _ => Ty::Any, // async def: suspension points hand Python objects to the event loop
            };
            self.fx().gen = Some(("gen_".into(), elem));
            if let Some(Expr { kind: ExprKind::Subscript { value, index }, .. }) = def.returns.as_ref() {
                if let (ExprKind::Name(n), ExprKind::Tuple { elts, .. }) = (&value.kind, &index.kind) {
                    if n == "Generator" && elts.len() >= 2 {
                        let m = self.cur_mod();
                        let subst = self.fxr().subst.clone();
                        let st = self.resolve_ann(&elts[1], m, &subst)?;
                        self.fx().gen_send = st;
                    }
                }
            }
        }
        // Pre-declare annotated locals.
        self.predeclare_annotated(&def.body)?;
        for st in &def.body {
            self.stmt(st)?;
        }
        // `...`-only bodies (Protocol members, abstract stubs) raise NotImplementedError if reached
        let is_stub = def.body.iter().all(|s| matches!(&s.kind, StmtKind::Expr(Expr { kind: ExprKind::Ellipsis | ExprKind::Str { .. }, .. }) | StmtKind::Pass))
            && def.body.iter().any(|s| matches!(&s.kind, StmtKind::Expr(Expr { kind: ExprKind::Ellipsis, .. })));
        if is_stub && !is_gen {
            self.emit(&format!("fpy_raise_code(FPY_EXC_NOTIMPL, {});", c_string(format!("{}() is abstract", def.name).as_bytes())));
        }
        if !is_stub && !is_gen && rett != Ty::NoneT && !rett.is_nullable() && !always_terminates(&def.body) && rett != Ty::Never {
            return Err(self
                .err(self.cur_mod(), Span { line: def.body.last().map(|s| s.end_line).unwrap_or(1), col: 1, start: 0, end: 0 }, format!("function '{}' is missing a return statement (declared to return {})", def.name, self.tyname(&rett)))
                .note("add a `return` on every path, or change the return annotation"));
        }
        let rett = if is_gen { Ty::NoneT } else { self.fxr().ret.clone() };
        if !is_gen && super::has_unknown(&rett) {
            return Err(self.e(Span::default(), format!("cannot infer the return type of '{}'", def.name)));
        }
        let ret_c = if rett == Ty::NoneT || rett == Ty::Never { "void".to_string() } else { self.ctype(&rett) };
        let gen_elem = self.fxr().gen.as_ref().map(|g| g.1.clone());
        let fx = self.fx();
        let mut body = String::new();
        for d in &fx.decls {
            body.push_str(&format!("    {}\n", d));
        }
        body.push_str(&fx.body);
        if ret_c != "void" {
            body.push_str(&format!("    return {};\n", rett.zero()));
        }
        if !is_gen {
            let proto = format!("static {} {}({});\n", ret_c, cname, params_c);
            let code = format!("static {} {}({}) {{\n{}}}\n\n", ret_c, cname, params_c, body);
            return Ok((proto, code));
        }
        // Generator: the public function packs arguments into a frame and returns the generator.
        let elem = gen_elem.unwrap();
        if super::has_unknown(&elem) {
            return Err(self.e(Span::default(), format!("cannot infer the element type of generator '{}'", def.name)));
        }
        let gb = format!("{}__gen", cname);
        let n = all_params.len() + 1;
        let mut kinds: Vec<u8> = vec![FK_OBJ];
        kinds.extend(all_params.iter().map(|(_, t)| t.kind()));
        let karr = self.kinds_array(&kinds);
        let mut pack = format!("    FpyTuple *fr = fpy_tuple_new({}, {});\n", n, karr);
        pack.push_str(&format!("    fr->items[0] = fpy_wp({});\n", if env.is_some() { "env_" } else { "NULL" }));
        let mut unpack = String::new();
        unpack.push_str("    FpyTuple *fr_ = (FpyTuple*)fpy_gen_frame(gen_);\n");
        if env.is_some() {
            unpack.push_str("    void *env_ = fpy_pw(fr_->items[0]);\n");
        }
        for (i, (pn, t)) in all_params.iter().enumerate() {
            let pc = format!("p_{}", c_ident(pn));
            pack.push_str(&format!("    fr->items[{}] = {};\n", i + 1, t.to_word(&pc)));
            let ct = self.ctype(t);
            unpack.push_str(&format!("    {} {} = {};\n", ct, pc, t.from_word(&format!("fr_->items[{}]", i + 1), &ct)));
        }
        let (proto, outer) = if def.is_async {
            (
                format!("static FpyObj *{}({});\nstatic void {}(FpyGen *gen_);\n", cname, params_c, gb),
                format!("static FpyObj *{}({}) {{\n{}    return fpy_coro_new(fpy_gen_new({}, fr, FK_OBJ), {});\n}}\n\n", cname, params_c, pack, gb, c_string(def.name.as_bytes())),
            )
        } else {
            (
                format!("static FpyIter *{}({});\nstatic void {}(FpyGen *gen_);\n", cname, params_c, gb),
                format!("static FpyIter *{}({}) {{\n{}    return fpy_gen_new({}, fr, {});\n}}\n\n", cname, params_c, pack, gb, elem.kind()),
            )
        };
        let inner = format!("static void {}(FpyGen *gen_) {{\n{}{}}}\n\n", gb, unpack, body);
        Ok((proto, format!("{}{}", outer, inner)))
    }

    fn predeclare_annotated(&mut self, body: &[Stmt]) -> R<()> {
        for st in body {
            match &st.kind {
                StmtKind::AnnAssign { target, ann, .. } => {
                    if let ExprKind::Name(n) = &target.kind {
                        if !self.fxr().locals.contains_key(n) && self.fxr().scope.locals.contains(n) {
                            let subst = self.fxr().subst.clone();
                            let t = self.resolve_ann(ann, self.cur_mod(), &subst)?;
                            self.declare_local(n, &t)?;
                        }
                    }
                }
                StmtKind::If { body, orelse, .. } | StmtKind::While { body, orelse, .. } | StmtKind::For { body, orelse, .. } => {
                    self.predeclare_annotated(body)?;
                    self.predeclare_annotated(orelse)?;
                }
                StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
                    self.predeclare_annotated(body)?;
                    for h in handlers {
                        self.predeclare_annotated(&h.body)?;
                    }
                    self.predeclare_annotated(orelse)?;
                    self.predeclare_annotated(finalbody)?;
                }
                StmtKind::With { body, .. } => self.predeclare_annotated(body)?,
                _ => {}
            }
        }
        Ok(())
    }

    pub fn declare_local(&mut self, name: &str, ty: &Ty) -> R<Local> {
        let cell = self.fxr().scope.cells.contains(name);
        let vol = self.fxr().scope.try_assigned.contains(name);
        let cname = if cell { format!("c_{}", c_ident(name)) } else { format!("v_{}", c_ident(name)) };
        let ct = self.ctype(ty);
        if cell {
            self.decl(format!("FpyCell *{} = fpy_cell_new({});", cname, ty.kind()));
        } else if vol {
            self.decl(format!("{} volatile {} = {};", ct, cname, ty.zero()));
        } else {
            self.decl(format!("{} {} = {};", ct, cname, ty.zero()));
        }
        let l = Local { ty: ty.clone(), cname, cell };
        self.fx().locals.insert(name.to_string(), l.clone());
        Ok(l)
    }

    // ------------------------------------------------------------------ variables

    /// Store a typed value into a name.
    pub fn store_name(&mut self, name: &str, val: CE, span: Span) -> R<()> {
        // invalidate narrowing
        self.fx().narrow.retain(|(n, _)| n != name);
        // comprehension variables
        for i in (0..self.fxr().comp.len()).rev() {
            if let Some(l) = self.fxr().comp[i].get(name).cloned() {
                let c = self.check_coerce(&val, &l.ty, span)?;
                self.emit(&format!("{} = {};", l.cname, c));
                return Ok(());
            }
        }
        let is_global = self.fxr().is_module || self.fxr().scope.globals.contains(name);
        if is_global {
            let m = self.cur_mod();
            let g = match self.modules[m].scope.get(name).cloned() {
                Some(Sym::Global(g)) => g,
                Some(_) if self.fxr().is_module => {
                    // rebinding a function/class name at module level is not allowed
                    return Err(self.e(span, format!("cannot assign to '{}': it names a function, class or module", name)));
                }
                _ => {
                    let gid = self.globals.len();
                    let cname = format!("g_{}_{}", c_ident(&self.modules[m].name), c_ident(name));
                    self.globals.push(GlobalVar { name: name.to_string(), cname, ty: None, module: m, decl_span: span, py_import: None });
                    self.modules[m].scope.insert(name.to_string(), Sym::Global(gid));
                    gid
                }
            };
            if self.globals[g].ty.is_none() {
                let t = self.infer_var_type(&val.ty, span, name)?;
                self.globals[g].ty = Some(t);
            }
            let gt = self.globals[g].ty.clone().unwrap();
            let c = self.check_coerce(&val, &gt, span)?;
            let gn = self.globals[g].cname.clone();
            self.emit(&format!("G.{} = {};", gn, c));
            return Ok(());
        }
        if let Some((slot, t)) = self.fxr().env.get(name).cloned() {
            if !self.fxr().scope.nonlocals.contains(name) {
                return Err(self.e(span, format!("cannot assign to captured variable '{}' without `nonlocal {}`", name, name)));
            }
            let c = self.check_coerce(&val, &t, span)?;
            self.emit(&format!("((FpyCell*)fpy_pw(env->items[{}]))->v = {};", slot, t.to_word(&c)));
            return Ok(());
        }
        let local = match self.fxr().locals.get(name).cloned() {
            Some(l) => l,
            None => {
                let t = self.infer_var_type(&val.ty, span, name)?;
                self.declare_local(name, &t)?
            }
        };
        let c = self.check_coerce(&val, &local.ty, span)?;
        if local.cell {
            self.emit(&format!("{}->v = {};", local.cname, local.ty.to_word(&c)));
        } else {
            self.emit(&format!("{} = {};", local.cname, c));
        }
        Ok(())
    }

    /// Check assignability and produce coerced C code.
    pub fn check_coerce(&mut self, val: &CE, to: &Ty, span: Span) -> R<String> {
        if has_unknown(&val.ty) {
            if let Some(c) = self.retype_empty(val, to) {
                return Ok(c);
            }
        }
        if !self.assignable(&val.ty, to) {
            return Err(self.e(span, format!("type mismatch: expected {}, got {}", self.tyname(to), self.tyname(&val.ty))));
        }
        Ok(self.coerce(&val.c, &val.ty, to))
    }

    /// Empty literals (`[]`, `{}`) are compiled with unknown element kinds; fix them up for a target type.
    fn retype_empty(&mut self, val: &CE, to: &Ty) -> Option<String> {
        match (&val.ty, to) {
            (Ty::List(_), Ty::List(e)) => Some(format!("fpy_list_new({}, 4)", e.kind())),
            (Ty::Dict(..), Ty::Dict(k, v)) => Some(format!("fpy_dict_new({}, {})", k.kind(), v.kind())),
            (Ty::Set(_), Ty::Set(e)) => Some(format!("fpy_set_new({})", e.kind())),
            (Ty::List(_), Ty::Optional(i)) | (Ty::Dict(..), Ty::Optional(i)) | (Ty::Set(_), Ty::Optional(i)) => self.retype_empty(val, i),
            (Ty::List(_), Ty::Iter(e)) => Some(format!("fpy_list_iter(fpy_list_new({}, 4))", e.kind())),
            _ => None,
        }
    }

    // ------------------------------------------------------------------ statements

    pub fn block(&mut self, body: &[Stmt]) -> R<()> {
        let saved = self.fxr().narrow.len();
        for st in body {
            self.stmt(st)?;
        }
        self.fx().narrow.truncate(saved);
        Ok(())
    }

    pub fn stmt(&mut self, st: &Stmt) -> R<()> {
        if self.is_alias_stmt(st) {
            return Ok(()); // `X = list[int]` / `X: TypeAlias = ...`: a type, not a value
        }
        match &st.kind {
            StmtKind::Pass => Ok(()),
            StmtKind::Expr(e) => {
                if let ExprKind::Str { .. } = e.kind {
                    return Ok(()); // docstring
                }
                if let ExprKind::Ellipsis = e.kind {
                    return Ok(());
                }
                let ce = self.expr(e, None)?;
                if ce.ty == Ty::NoneT && (ce.c.ends_with(')') || ce.c.ends_with("})")) {
                    self.emit(&format!("{};", ce.c));
                } else {
                    self.emit(&format!("(void)({});", ce.c));
                }
                Ok(())
            }
            StmtKind::Assign { targets, value } => {
                if targets.len() == 1 {
                    if self.dict_update_peephole(&targets[0], value)? {
                        return Ok(());
                    }
                    // swap/parallel assignment without allocating a tuple
                    if let (ExprKind::Tuple { elts: te, .. }, ExprKind::Tuple { elts: ve, .. }) = (&targets[0].kind, &value.kind) {
                        if te.len() == ve.len() && !te.iter().any(|t| matches!(t.kind, ExprKind::Starred(_))) {
                            let mut temps = Vec::new();
                            for (t, v) in te.iter().zip(ve) {
                                let want = self.target_type(t);
                                let ce = self.expr(v, want.as_ref())?;
                                let tmp = self.tmp_var(&ce.ty, "t");
                                self.emit(&format!("{} = {};", tmp, ce.c));
                                temps.push(CE::new(ce.ty, tmp));
                            }
                            for (t, ce) in te.iter().zip(temps) {
                                self.store_target(t, ce)?;
                            }
                            return Ok(());
                        }
                    }
                }
                let want = if targets.len() == 1 { self.target_type(&targets[0]) } else { None };
                let ce = self.expr(value, want.as_ref())?;
                if targets.len() == 1 {
                    return self.store_target(&targets[0], ce);
                }
                let tmp = self.tmp_var(&ce.ty, "t");
                self.emit(&format!("{} = {};", tmp, ce.c));
                for t in targets {
                    self.store_target(t, CE::new(ce.ty.clone(), tmp.clone()))?;
                }
                Ok(())
            }
            StmtKind::AnnAssign { target, ann, value } => {
                let subst = self.fxr().subst.clone();
                let ty = self.resolve_ann(ann, self.cur_mod(), &subst)?;
                if let ExprKind::Name(n) = &target.kind {
                    if self.fxr().is_module || self.fxr().scope.globals.contains(n) {
                        let m = self.cur_mod();
                        if let Some(Sym::Global(g)) = self.modules[m].scope.get(n).cloned() {
                            if self.globals[g].ty.is_none() {
                                self.globals[g].ty = Some(ty.clone());
                            }
                        }
                    } else if !self.fxr().locals.contains_key(n) {
                        self.declare_local(n, &ty)?;
                    } else if self.fxr().locals[n].ty != ty {
                        return Err(self.e(target.span, format!("'{}' was already declared as {}", n, self.tyname(&self.fxr().locals[n].ty.clone()))));
                    }
                }
                if let Some(v) = value {
                    let ce = self.expr(v, Some(&ty))?;
                    let c = self.check_coerce(&ce, &ty, v.span)?;
                    self.store_target(target, CE::new(ty, c))?;
                } else if let ExprKind::Attribute { .. } = target.kind {
                    return Err(self.e(target.span, "attribute annotations need a value"));
                }
                Ok(())
            }
            StmtKind::AugAssign { target, op, value } => self.aug_assign(target, *op, value, st.span),
            StmtKind::Return(v) => self.return_stmt(v.as_ref(), st.span),
            StmtKind::If { test, body, orelse } => self.if_stmt(test, body, orelse),
            StmtKind::While { test, body, orelse } => {
                let broke = if orelse.is_empty() { None } else { Some(self.tmp_var(&Ty::Bool, "broke")) };
                let cond = self.cond(test)?;
                self.emit(&format!("while ({}) {{", cond.0));
                let depth = self.fxr().cleanup.len();
                self.fx().loops.push(LoopCtx { cleanup_depth: depth, broke: broke.clone() });
                self.fx().indent += 1;
                let saved = self.fxr().narrow.len();
                self.fx().narrow.extend(cond.1);
                self.block(body)?;
                self.fx().narrow.truncate(saved);
                self.fx().indent -= 1;
                self.fx().loops.pop();
                self.emit("}");
                if let Some(b) = broke {
                    self.emit(&format!("if (!{}) {{", b));
                    self.fx().indent += 1;
                    self.block(orelse)?;
                    self.fx().indent -= 1;
                    self.emit("}");
                }
                Ok(())
            }
            StmtKind::For { target, iter, body, is_async: true, .. } => self.async_for(target, iter, body, st.span),
            StmtKind::For { target, iter, body, orelse, .. } => {
                let broke = if orelse.is_empty() { None } else { Some(self.tmp_var(&Ty::Bool, "broke")) };
                let b = body.clone();
                self.for_loop(target, iter, broke.clone(), &mut |c: &mut Compiler| c.block(&b))?;
                if let Some(b) = broke {
                    self.emit(&format!("if (!{}) {{", b));
                    self.fx().indent += 1;
                    self.block(orelse)?;
                    self.fx().indent -= 1;
                    self.emit("}");
                }
                Ok(())
            }
            StmtKind::Break => {
                let lp = self.fxr().loops.last().cloned().ok_or_else(|| self.e(st.span, "'break' outside loop"))?;
                self.unwind_to(lp.cleanup_depth)?;
                if let Some(b) = lp.broke {
                    self.emit(&format!("{} = true;", b));
                }
                self.emit("break;");
                Ok(())
            }
            StmtKind::Continue => {
                let lp = self.fxr().loops.last().cloned().ok_or_else(|| self.e(st.span, "'continue' not properly in loop"))?;
                self.unwind_to(lp.cleanup_depth)?;
                self.emit("continue;");
                Ok(())
            }
            StmtKind::Raise { exc, cause } => self.raise_stmt(exc.as_ref(), cause.as_ref(), st.span),
            StmtKind::Try { body, handlers, orelse, finalbody, .. } => self.try_stmt(body, handlers, orelse, finalbody),
            StmtKind::With { items, body, is_async } => self.with_stmt(items, body, st.span, *is_async),
            StmtKind::Assert { test, msg } => {
                let (c, _) = self.cond(test)?;
                let m = match msg {
                    Some(m) => {
                        let ce = self.expr(m, None)?;
                        self.to_str(&ce)
                    }
                    None => self.exc_no_arg(),
                };
                let ae = self.builtin_class("AssertionError").unwrap();
                let cn = self.class_cname(ae);
                self.emit(&format!("if (!({})) fpy_raise((FpyObj*)new_{}((FpyObj*){}));", c, cn, m));
                Ok(())
            }
            StmtKind::Del(targets) => {
                for t in targets {
                    self.del_target(t)?;
                }
                Ok(())
            }
            StmtKind::Global(_) | StmtKind::Nonlocal(_) => Ok(()),
            StmtKind::Import(_) | StmtKind::ImportFrom { .. } => {
                if self.fxr().is_module {
                    // module-level imports were bound in the declaration pass; run inits.
                    self.emit_import_inits(st)?;
                    return Ok(());
                }
                // function-level import: bind into module scope view
                let m = self.cur_mod();
                let mut binds = Vec::new();
                self.process_import(m, st, &mut binds)?;
                for (k, v) in &binds {
                    if !self.modules[m].scope.contains_key(k) {
                        self.modules[m].scope.insert(k.clone(), v.clone());
                    }
                }
                self.emit_import_inits(st)?;
                // imported variables are locals of this function (the scope pass saw the binding)
                for (k, v) in binds {
                    if let Sym::Global(g) = v {
                        if self.fxr().scope.locals.contains(&k) {
                            let ty = self.globals[g].ty.clone().unwrap_or(Ty::Any);
                            let val = CE::new(ty.clone(), format!("G.{}", self.globals[g].cname));
                            if !self.fxr().locals.contains_key(&k) {
                                self.declare_local(&k, &ty)?;
                            }
                            self.store_name(&k, val, st.span)?;
                        }
                    }
                }
                Ok(())
            }
            StmtKind::FuncDef(fd) => {
                if self.fxr().is_module {
                    if fd.type_params.is_empty() && !self.fxr().dry_run {
                        if let Some(Sym::Func(fid)) = self.modules[self.cur_mod()].scope.get(&fd.name).cloned() {
                            if self.funcs[fid].extern_name.is_none() {
                                self.func_sig(fid)?;
                                self.func_instance(fid, vec![])?;
                                self.apply_dynamic_decorators(fid, fd)?;
                            }
                        }
                    }
                    return Ok(());
                }
                self.nested_function(fd, st.span)
            }
            StmtKind::ClassDef(cd) => {
                if !self.fxr().is_module {
                    return Err(self.e(st.span, "classes must be defined at module level"));
                }
                if self.fxr().dry_run {
                    return Ok(());
                }
                if let Some(Sym::Class(cid)) = self.modules[self.cur_mod()].scope.get(&cd.name).cloned() {
                    self.class_init_stmts(cid)?;
                }
                if let Some(pi) = self.py_classes.iter().position(|c| c.def.name == cd.name && c.module == self.cur_mod()) {
                    self.emit_py_class(pi)?;
                }
                Ok(())
            }
            StmtKind::TypeAlias { .. } => Ok(()),
            StmtKind::Match { subject, cases } => self.match_stmt(subject, cases),
        }
    }

    fn emit_import_inits(&mut self, st: &Stmt) -> R<()> {
        let names: Vec<String> = match &st.kind {
            StmtKind::Import(names) => names.iter().map(|a| a.name.clone()).collect(),
            StmtKind::ImportFrom { module, level, names } => {
                let mut v = vec![self.resolve_relative(self.cur_mod(), *level, module)];
                for a in names {
                    v.push(format!("{}.{}", v[0], a.name));
                }
                v
            }
            _ => vec![],
        };
        // Python (CPython fallback) imports: bind globals at this point of execution
        let m = self.cur_mod();
        let binds: Vec<String> = match &st.kind {
            StmtKind::Import(names) => names.iter().map(|a| a.asname.clone().unwrap_or_else(|| a.name.split('.').next().unwrap().to_string())).collect(),
            StmtKind::ImportFrom { names, .. } => names.iter().map(|a| a.asname.clone().unwrap_or_else(|| a.name.clone())).collect(),
            _ => vec![],
        };
        for b in binds {
            if let Some(Sym::Global(g)) = self.modules[m].scope.get(&b).cloned() {
                if let Some(imp) = self.globals[g].py_import.clone() {
                    let gn = self.globals[g].cname.clone();
                    let val = match &imp.attr {
                        None => format!("fpy_py_import({}, {})", c_string(imp.module.as_bytes()), imp.top_level),
                        Some(a) => format!("fpy_py_from_import({}, {})", c_string(imp.module.as_bytes()), c_string(a.as_bytes())),
                    };
                    self.emit(&format!("G.{} = {};", gn, val));
                    self.emit(&format!("fpy_py_publish({}, G.{});", c_string(b.as_bytes()), gn));
                }
            }
        }
        for n in names {
            // init parent packages too
            let parts: Vec<&str> = n.split('.').collect();
            for i in 1..=parts.len() {
                let p = parts[..i].join(".");
                if let Some(&id) = self.mod_index.get(&p) {
                    let init = self.modules[id].init_cname.clone();
                    self.emit(&format!("{}();", init));
                }
            }
        }
        Ok(())
    }

    /// The declared type of an assignment target, if known (for bidirectional inference).
    pub fn target_type(&mut self, t: &Expr) -> Option<Ty> {
        match &t.kind {
            ExprKind::Name(n) => {
                for c in self.fxr().comp.iter().rev() {
                    if let Some(l) = c.get(n) {
                        return Some(l.ty.clone());
                    }
                }
                if let Some(l) = self.fxr().locals.get(n) {
                    return Some(l.ty.clone());
                }
                if let Some((_, t)) = self.fxr().env.get(n) {
                    return Some(t.clone());
                }
                if self.fxr().is_module || self.fxr().scope.globals.contains(n) {
                    if let Some(Sym::Global(g)) = self.modules[self.cur_mod()].scope.get(n) {
                        return self.globals[*g].ty.clone();
                    }
                }
                None
            }
            ExprKind::Attribute { value, attr } => {
                let saved = self.fxr().dry_run;
                self.fx().dry_run = true;
                let r = self.expr(value, None);
                self.fx().dry_run = saved;
                match r.ok()?.ty {
                    Ty::Class(c) => self.find_field(c, attr).map(|f| f.ty),
                    _ => None,
                }
            }
            ExprKind::Subscript { value, .. } => {
                let saved = self.fxr().dry_run;
                self.fx().dry_run = true;
                let r = self.expr(value, None);
                self.fx().dry_run = saved;
                match r.ok()?.ty {
                    Ty::List(e) => Some(*e),
                    Ty::Dict(_, v) => Some(*v),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub fn store_target(&mut self, t: &Expr, val: CE) -> R<()> {
        match &t.kind {
            ExprKind::Name(n) => self.store_name(n, val, t.span),
            ExprKind::Attribute { value, attr } => {
                let obj = self.expr(value, None)?;
                match &obj.ty {
                    Ty::Class(c) => {
                        let c = *c;
                        if self.classes[c].dc_frozen && self.fxr().fname != format!("{}.__init__", self.classes[c].name) {
                            return Err(self.e(t.span, format!("cannot assign to field '{}' of frozen dataclass", attr)));
                        }
                        if let Some(f) = self.find_field(c, attr) {
                            let cv = self.check_coerce(&val, &f.ty, t.span)?;
                            self.emit(&format!("({})->f_{} = {};", obj.c, c_ident(attr), cv));
                            return Ok(());
                        }
                        if self.find_class_var(c, attr).is_some() {
                            // CPython would create an instance attribute shadowing the class variable
                            let cn = self.classes[c].name.clone();
                            return Err(self
                                .e(t.span, format!("cannot assign to class variable '{}' through an instance", attr))
                                .note(format!("assign `{}.{} = ...` to change the class variable, or declare an instance field", cn, attr)));
                        }
                        Err(self.e(t.span, format!("'{}' has no attribute '{}' (declare it in the class body)", self.classes[c].name, attr)))
                    }
                    Ty::Type(c) => {
                        if let Some(g) = self.find_class_var(*c, attr) {
                            let gt = self.globals[g].ty.clone().unwrap_or(val.ty.clone());
                            let cv = self.check_coerce(&val, &gt, t.span)?;
                            let gn = self.globals[g].cname.clone();
                            self.emit(&format!("G.{} = {};", gn, cv));
                            return Ok(());
                        }
                        Err(self.e(t.span, format!("class has no attribute '{}'", attr)))
                    }
                    Ty::Any => {
                        let v = self.coerce(&val.c, &val.ty, &Ty::Any);
                        self.emit(&format!("fpy_dyn_setattr({}, {}, {});", obj.c, c_string(attr.as_bytes()), v));
                        Ok(())
                    }
                    _ => Err(self.e(t.span, format!("cannot set attribute on {}", self.tyname(&obj.ty)))),
                }
            }
            ExprKind::Subscript { value, index } => {
                let obj = self.expr(value, None)?;
                match obj.ty.clone() {
                    Ty::List(e) => {
                        if let ExprKind::Slice { lower, upper, step } = &index.kind {
                            if step.is_some() {
                                return Err(self.e(t.span, "extended slice assignment is not supported"));
                            }
                            let (hl, l) = self.opt_int(lower.as_deref())?;
                            let (hu, u) = self.opt_int(upper.as_deref())?;
                            let v = self.check_coerce(&val, &obj.ty, t.span)?;
                            self.emit(&format!("fpy_list_set_slice({}, {}, {}, {}, {}, {});", obj.c, hl, l, hu, u, v));
                            return Ok(());
                        }
                        let i = self.expr(index, Some(&Ty::Int))?;
                        let ic = self.check_coerce(&i, &Ty::Int, index.span)?;
                        let v = self.check_coerce(&val, &e, t.span)?;
                        self.emit(&format!("fpy_list_set({}, {}, {});", obj.c, ic, e.to_word(&v)));
                        Ok(())
                    }
                    Ty::Dict(k, v) => {
                        let kc = self.expr(index, Some(&k))?;
                        let kc = self.check_coerce(&kc, &k, index.span)?;
                        let vc = self.check_coerce(&val, &v, t.span)?;
                        self.emit(&format!("fpy_dict_set({}, {}, {});", obj.c, k.to_word(&kc), v.to_word(&vc)));
                        Ok(())
                    }
                    Ty::Class(c) => {
                        let f = self.find_method(c, "__setitem__").ok_or_else(|| self.e(t.span, format!("'{}' does not support item assignment", self.classes[c].name)))?;
                        let call = self.call_method_ce(obj, f, vec![(**index).clone()], Some(val), t.span)?;
                        self.emit(&format!("{};", call.c));
                        Ok(())
                    }
                    Ty::Any => {
                        let k = self.expr(index, None)?;
                        let kc = self.coerce(&k.c, &k.ty, &Ty::Any);
                        let vc = self.coerce(&val.c, &val.ty, &Ty::Any);
                        self.emit(&format!("fpy_dyn_setitem({}, {}, {});", obj.c, kc, vc));
                        Ok(())
                    }
                    t2 => Err(self.e(t.span, format!("'{}' does not support item assignment", self.tyname(&t2)))),
                }
            }
            ExprKind::Tuple { elts, .. } | ExprKind::List(elts) => self.unpack(elts, val, t.span),
            ExprKind::Starred(_) => Err(self.e(t.span, "starred assignment target must be in a list or tuple")),
            _ => Err(self.e(t.span, "cannot assign to this expression")),
        }
    }

    fn unpack(&mut self, elts: &[Expr], val: CE, span: Span) -> R<()> {
        // tuple[T, ...] unpacks like a list (a starred target becomes a list, as in Python)
        let val = match &val.ty {
            Ty::VarTuple(e) => CE::new(Ty::List(e.clone()), format!("({{ FpyTuple *_vt = {}; fpy_list_from({}, _vt->len, _vt->items); }})", val.c, e.kind())),
            _ => val,
        };
        let star = elts.iter().position(|e| matches!(e.kind, ExprKind::Starred(_)));
        match val.ty.clone() {
            Ty::Tuple(ts) => {
                if star.is_some() {
                    return Err(self.e(span, "starred unpacking of tuples is not supported; unpack a list"));
                }
                if ts.len() != elts.len() {
                    return Err(self.e(span, format!("cannot unpack a {}-tuple into {} targets", ts.len(), elts.len())));
                }
                let tmp = self.tmp_var(&val.ty, "u");
                self.emit(&format!("{} = {};", tmp, val.c));
                for (i, (e, t)) in elts.iter().zip(ts.iter()).enumerate() {
                    let ct = self.ctype(t);
                    let item = t.from_word(&format!("{}->items[{}]", tmp, i), &ct);
                    self.store_target(e, CE::new(t.clone(), item))?;
                }
                Ok(())
            }
            Ty::List(et) => {
                let tmp = self.tmp_var(&val.ty, "u");
                self.emit(&format!("{} = {};", tmp, val.c));
                let ct = self.ctype(&et);
                match star {
                    None => {
                        self.emit(&format!(
                            "if ({t}->len != {n}) fpy_raise_fmt(FPY_EXC_VALUE, {t}->len > {n} ? \"too many values to unpack (expected %d, got %lld)\" : \"not enough values to unpack (expected %d, got %lld)\", {n}, (long long){t}->len);",
                            t = tmp,
                            n = elts.len()
                        ));
                        for (i, e) in elts.iter().enumerate() {
                            let item = et.from_word(&format!("{}->items[{}]", tmp, i), &ct);
                            self.store_target(e, CE::new((*et).clone(), item))?;
                        }
                    }
                    Some(si) => {
                        let after = elts.len() - si - 1;
                        self.emit(&format!(
                            "if ({}->len < {}) fpy_raise_fmt(FPY_EXC_VALUE, \"not enough values to unpack (expected at least %d, got %lld)\", {}, (long long){}->len);",
                            tmp,
                            elts.len() - 1,
                            elts.len() - 1,
                            tmp
                        ));
                        for (i, e) in elts.iter().enumerate() {
                            if i < si {
                                let item = et.from_word(&format!("{}->items[{}]", tmp, i), &ct);
                                self.store_target(e, CE::new((*et).clone(), item))?;
                            } else if i == si {
                                let inner = match &e.kind {
                                    ExprKind::Starred(x) => x,
                                    _ => unreachable!(),
                                };
                                let sl = format!("fpy_list_slice({}, true, {}, true, {}->len - {}, 1)", tmp, si, tmp, after);
                                self.store_target(inner, CE::new(val.ty.clone(), sl))?;
                            } else {
                                let item = et.from_word(&format!("{}->items[{}->len - {}]", tmp, tmp, elts.len() - i), &ct);
                                self.store_target(e, CE::new((*et).clone(), item))?;
                            }
                        }
                    }
                }
                Ok(())
            }
            Ty::Any => {
                let tmp = self.tmp_var(&Ty::list(Ty::Any), "u");
                self.emit(&format!("{} = fpy_list_from_iter(FK_OBJ, fpy_dyn_iter({}));", tmp, val.c));
                self.unpack(elts, CE::new(Ty::list(Ty::Any), tmp), span)
            }
            t => Err(self.e(span, format!("cannot unpack {}", self.tyname(&t)))),
        }
    }

    fn del_target(&mut self, t: &Expr) -> R<()> {
        match &t.kind {
            ExprKind::Subscript { value, index } => {
                let obj = self.expr(value, None)?;
                match obj.ty.clone() {
                    Ty::List(_) => {
                        if let ExprKind::Slice { lower, upper, .. } = &index.kind {
                            let (hl, l) = self.opt_int(lower.as_deref())?;
                            let (hu, u) = self.opt_int(upper.as_deref())?;
                            self.emit(&format!("fpy_list_del_slice({}, {}, {}, {}, {});", obj.c, hl, l, hu, u));
                            return Ok(());
                        }
                        let i = self.expr(index, Some(&Ty::Int))?;
                        let ic = self.check_coerce(&i, &Ty::Int, index.span)?;
                        self.emit(&format!("fpy_list_del({}, {});", obj.c, ic));
                        Ok(())
                    }
                    Ty::Dict(k, _) => {
                        let kc = self.expr(index, Some(&k))?;
                        let kc = self.check_coerce(&kc, &k, index.span)?;
                        self.emit(&format!("fpy_dict_del({}, {}, true);", obj.c, k.to_word(&kc)));
                        Ok(())
                    }
                    other => Err(self.e(t.span, format!("cannot delete items of {}", self.tyname(&other)))),
                }
            }
            ExprKind::Name(_) => Ok(()),
            _ => Err(self.e(t.span, "unsupported del target")),
        }
    }

    pub fn opt_int(&mut self, e: Option<&Expr>) -> R<(String, String)> {
        match e {
            None => Ok(("false".into(), "0".into())),
            Some(e) => {
                let ce = self.expr(e, Some(&Ty::Int))?;
                let c = self.check_coerce(&ce, &Ty::Int, e.span)?;
                Ok(("true".into(), c))
            }
        }
    }

    /// `d[k] = d.get(k, D) OP v` → a single hash probe on the common (present) path.
    fn dict_update_peephole(&mut self, target: &Expr, value: &Expr) -> R<bool> {
        let (ExprKind::Subscript { value: d1, index: k1 }, ExprKind::BinOp { left, op, right }) = (&target.kind, &value.kind) else { return Ok(false) };
        let ExprKind::Call { func, args, keywords } = &left.kind else { return Ok(false) };
        let ExprKind::Attribute { value: d2, attr } = &func.kind else { return Ok(false) };
        if attr != "get" || args.len() != 2 || !keywords.is_empty() {
            return Ok(false);
        }
        let same = |a: &Expr, b: &Expr| matches!((&a.kind, &b.kind), (ExprKind::Name(x), ExprKind::Name(y)) if x == y);
        if !same(d1, d2) || !same(k1, &args[0]) {
            return Ok(false);
        }
        let d = self.expr(d1, None)?;
        let Ty::Dict(kt, vt) = d.ty.clone() else { return Ok(false) };
        if !vt.is_scalar() {
            return Ok(false);
        }
        self.dict_slot_update(&d, &kt, &vt, k1, Some(&args[1]), *op, right, value.span)?;
        Ok(true)
    }

    /// Shared code for `d[k] op= v` and the get-default idiom.
    #[allow(clippy::too_many_arguments)]
    fn dict_slot_update(&mut self, d: &CE, kt: &Ty, vt: &Ty, key: &Expr, default: Option<&Expr>, op: BinOp, rhs: &Expr, span: Span) -> R<()> {
        let k = self.expr(key, Some(kt))?;
        let kc = self.check_coerce(&k, kt, key.span)?;
        let r = self.expr(rhs, Some(vt))?;
        let dv = self.tmp_var(&d.ty, "d");
        let kv = self.tmp_var(kt, "k");
        let rv = self.tmp_var(&r.ty, "r");
        self.emit(&format!("{} = {}; {} = {}; {} = {};", dv, d.c, kv, kc, rv, r.c));
        let slot = self.fresh("slot");
        self.decl(format!("fpy_word *{} = NULL;", slot));
        self.emit(&format!("{} = fpy_dict_slot({}, {});", slot, dv, kt.to_word(&kv)));
        let vct = self.ctype(vt);
        let cur = match default {
            Some(dflt) => {
                let dc = self.expr(dflt, Some(vt))?;
                let dc = self.check_coerce(&dc, vt, dflt.span)?;
                format!("({} ? {} : ({}))", slot, vt.from_word(&format!("*{}", slot), &vct), dc)
            }
            None => {
                self.emit(&format!("if (!{}) fpy_dict_get({}, {});", slot, dv, kt.to_word(&kv)));
                vt.from_word(&format!("*{}", slot), &vct)
            }
        };
        let res = self.binop_ce(CE::new(vt.clone(), cur), op, CE::new(r.ty.clone(), rv), span)?;
        let rc = self.check_coerce(&res, vt, span)?;
        let nv = self.tmp_var(vt, "nv");
        self.emit(&format!("{} = {};", nv, rc));
        self.emit(&format!("if ({s}) *{s} = {w}; else fpy_dict_set({d}, {k}, {w});", s = slot, w = vt.to_word(&nv), d = dv, k = kt.to_word(&kv)));
        Ok(())
    }

    fn aug_assign(&mut self, target: &Expr, op: BinOp, value: &Expr, span: Span) -> R<()> {
        // d[k] op= v on scalar-valued dicts: single probe
        if let ExprKind::Subscript { value: dv, index } = &target.kind {
            if !matches!(index.kind, ExprKind::Slice { .. }) {
                let saved = self.fxr().dry_run;
                self.fx().dry_run = true;
                let probe = self.expr(dv, None);
                self.fx().dry_run = saved;
                if let Ok(CE { ty: Ty::Dict(kt, vt), .. }) = probe {
                    if vt.is_scalar() {
                        let d = self.expr(dv, None)?;
                        return self.dict_slot_update(&d, &kt, &vt, index, None, op, value, span);
                    }
                }
            }
        }
        // In-place list extension keeps aliasing semantics (`xs += ys` mutates xs).
        let tt = self.target_type(target);
        if let (Some(Ty::List(e)), BinOp::Add) = (&tt, op) {
            let lhs = self.expr(target, None)?;
            let rhs = self.expr(value, Some(&Ty::List(e.clone())))?;
            match &rhs.ty {
                Ty::List(_) => {
                    let rc = self.check_coerce(&rhs, &Ty::List(e.clone()), value.span)?;
                    self.emit(&format!("fpy_list_extend({}, {});", lhs.c, rc));
                }
                _ => {
                    let rc = self.check_coerce(&rhs, &Ty::iter((**e).clone()), value.span)?;
                    self.emit(&format!("fpy_list_extend_iter({}, {});", lhs.c, rc));
                }
            }
            return Ok(());
        }
        match &target.kind {
            ExprKind::Name(_) => {
                let cur = self.expr(target, None)?;
                let rhs = self.expr(value, Some(&cur.ty))?;
                let res = self.binop_ce(cur, op, rhs, span)?;
                self.store_target(target, res)
            }
            ExprKind::Attribute { value: obj, attr } => {
                let o = self.expr(obj, None)?;
                let tmp = self.tmp_var(&o.ty, "a");
                self.emit(&format!("{} = {};", tmp, o.c));
                let objexpr = Expr::new(ExprKind::Name(format!("\u{1}{}", tmp)), obj.span);
                self.fx().comp.push(HashMap::from([(format!("\u{1}{}", tmp), Local { ty: o.ty.clone(), cname: tmp.clone(), cell: false })]));
                let t2 = Expr::new(ExprKind::Attribute { value: Box::new(objexpr), attr: attr.clone() }, target.span);
                let r = (|| -> R<()> {
                    let cur = self.expr(&t2, None)?;
                    let rhs = self.expr(value, Some(&cur.ty))?;
                    let res = self.binop_ce(cur, op, rhs, span)?;
                    self.store_target(&t2, res)
                })();
                self.fx().comp.pop();
                r
            }
            ExprKind::Subscript { value: obj, index } => {
                let o = self.expr(obj, None)?;
                let ot = self.tmp_var(&o.ty, "a");
                self.emit(&format!("{} = {};", ot, o.c));
                let want = match &o.ty {
                    Ty::Dict(k, _) => Some((**k).clone()),
                    Ty::List(_) => Some(Ty::Int),
                    _ => None,
                };
                let k = self.expr(index, want.as_ref())?;
                let kt = self.tmp_var(&k.ty, "k");
                self.emit(&format!("{} = {};", kt, k.c));
                let on = format!("\u{1}{}", ot);
                let kn = format!("\u{1}{}", kt);
                self.fx().comp.push(HashMap::from([
                    (on.clone(), Local { ty: o.ty.clone(), cname: ot.clone(), cell: false }),
                    (kn.clone(), Local { ty: k.ty.clone(), cname: kt.clone(), cell: false }),
                ]));
                let t2 = Expr::new(
                    ExprKind::Subscript { value: Box::new(Expr::new(ExprKind::Name(on), obj.span)), index: Box::new(Expr::new(ExprKind::Name(kn), index.span)) },
                    target.span,
                );
                let r = (|| -> R<()> {
                    let cur = self.expr(&t2, None)?;
                    let rhs = self.expr(value, Some(&cur.ty))?;
                    let res = self.binop_ce(cur, op, rhs, span)?;
                    self.store_target(&t2, res)
                })();
                self.fx().comp.pop();
                r
            }
            _ => Err(self.e(span, "invalid augmented assignment target")),
        }
    }

    fn return_stmt(&mut self, v: Option<&Expr>, span: Span) -> R<()> {
        if self.fxr().is_module {
            return Err(self.e(span, "'return' outside function"));
        }
        if let Some(rt) = self.fxr().async_ret.clone() {
            if let Some(e) = v {
                let ce = self.expr(e, Some(&rt))?;
                let c = self.check_coerce(&ce, &rt, e.span)?;
                let boxed = self.coerce(&c, &rt, &Ty::Any);
                self.emit(&format!("fpy_gen_set_result(gen_, fpy_wp({}));", boxed));
            }
            self.unwind_to(0)?;
            self.emit("return;");
            return Ok(());
        }
        if self.fxr().gen.is_some() {
            if v.is_some() {
                return Err(self.e(span, "generators cannot return a value"));
            }
            self.unwind_to(0)?;
            self.emit("return;");
            return Ok(());
        }
        let ret = self.fxr().ret.clone();
        match v {
            None => {
                if ret != Ty::NoneT && !ret.is_nullable() {
                    return Err(self.e(span, format!("function must return {}", self.tyname(&ret))));
                }
                self.unwind_to(0)?;
                if ret == Ty::NoneT {
                    self.emit("return;");
                } else {
                    self.emit("return NULL;");
                }
            }
            Some(e) => {
                let ret = if has_unknown(&ret) {
                    let saved = self.fxr().dry_run;
                    self.fx().dry_run = true;
                    let ce = self.expr(e, None);
                    self.fx().dry_run = saved;
                    let ce = ce?;
                    let t = self.infer_var_type(&ce.ty, e.span, "<return>")?;
                    self.fx().ret = t.clone();
                    t
                } else {
                    ret
                };
                let ce = self.expr(e, Some(&ret))?;
                if ret == Ty::NoneT {
                    if ce.ty != Ty::NoneT {
                        return Err(self.e(e.span, format!("function declared to return None returns {}", self.tyname(&ce.ty))));
                    }
                    if !matches!(e.kind, ExprKind::NoneLit) {
                        self.emit(&format!("{};", ce.c));
                    }
                    self.unwind_to(0)?;
                    self.emit("return;");
                    return Ok(());
                }
                let c = self.check_coerce(&ce, &ret, e.span)?;
                if self.fxr().cleanup.is_empty() {
                    self.emit(&format!("return {};", c));
                } else {
                    let t = self.tmp_var(&ret, "r");
                    self.emit(&format!("{} = {};", t, c));
                    self.unwind_to(0)?;
                    self.emit(&format!("return {};", t));
                }
            }
        }
        Ok(())
    }

    /// Emit cleanup code (pop exception frames, run finally blocks) down to `depth`.
    fn unwind_to(&mut self, depth: usize) -> R<()> {
        let entries: Vec<Cleanup> = self.fxr().cleanup[depth..].to_vec();
        for (i, c) in entries.iter().enumerate().rev() {
            match c {
                Cleanup::Frame(f) => self.emit(&format!("FPY_POP({});", f)),
                Cleanup::Finally(body) => {
                    // run the finally block outside of its own cleanup entry
                    let saved: Vec<Cleanup> = self.fx().cleanup.drain(depth + i..).collect();
                    let r = self.block(body);
                    self.fx().cleanup.extend(saved);
                    r?;
                }
            }
        }
        Ok(())
    }

    fn if_stmt(&mut self, test: &Expr, body: &[Stmt], orelse: &[Stmt]) -> R<()> {
        let (c, pos, neg) = self.cond3(test)?;
        self.emit(&format!("if ({}) {{", c));
        self.fx().indent += 1;
        let saved = self.fxr().narrow.len();
        self.fx().narrow.extend(pos);
        self.block(body)?;
        self.fx().narrow.truncate(saved);
        self.fx().indent -= 1;
        if !orelse.is_empty() {
            self.emit("} else {");
            self.fx().indent += 1;
            self.fx().narrow.extend(neg.clone());
            self.block(orelse)?;
            self.fx().narrow.truncate(saved);
            self.fx().indent -= 1;
        }
        self.emit("}");
        // Early-exit narrowing: `if x is None: return` narrows x afterwards.
        if orelse.is_empty() && always_terminates(body) || (!orelse.is_empty() && always_terminates(body) && !always_terminates(orelse) && false) {
            self.fx().narrow.extend(neg);
        }
        Ok(())
    }

    fn raise_stmt(&mut self, exc: Option<&Expr>, cause: Option<&Expr>, span: Span) -> R<()> {
        // `raise X from Y` / implicit chaining inside an except block: set __cause__ / __context__
        if let (Some(e), true) = (exc, cause.is_some() || !self.fxr().handler_exc.is_empty()) {
            let ce = self.expr(e, None)?;
            let base = self.exc_base_ty();
            let inst = match &ce.ty {
                Ty::Type(c) => self.construct(*c, vec![], vec![], span)?,
                Ty::Class(_) if self.assignable(&ce.ty, &base) => ce,
                _ => return self.raise_stmt_plain(Some(e), span),
            };
            let bc = self.classes[match &base { Ty::Class(c) => *c, _ => unreachable!() }].cname.clone();
            let t = self.fresh("rx");
            self.decl(format!("struct {} *{} = NULL;", bc, t));
            self.emit(&format!("{} = (struct {}*)({});", t, bc, inst.c));
            if let Some(c) = cause {
                let cv = self.expr(c, None)?;
                let cc = match &cv.ty {
                    Ty::NoneT => "NULL".to_string(),
                    Ty::Type(k) => self.construct(*k, vec![], vec![], span)?.c,
                    _ => self.check_coerce(&cv, &Ty::Optional(Box::new(base.clone())), span)?,
                };
                self.emit(&format!("{}->f___cause__ = (struct {}*)({});", t, bc, cc));
            }
            if let Some(h) = self.fxr().handler_exc.last().cloned() {
                self.emit(&format!("if ((FpyObj*){t} != (FpyObj*)({h}) && fpy_isinstance_t((FpyObj*)({h}), &T_{bc})) {t}->f___context__ = (struct {bc}*)({h});", t = t, h = h, bc = bc));
            }
            self.emit(&format!("fpy_raise((FpyObj*){});", t));
            return Ok(());
        }
        self.raise_stmt_plain(exc, span)
    }

    fn raise_stmt_plain(&mut self, exc: Option<&Expr>, span: Span) -> R<()> {
        match exc {
            None => {
                match self.fxr().handler_exc.last().cloned() {
                    Some(h) => self.emit(&format!("fpy_raise({});", h)),
                    None => self.emit("fpy_reraise();"),
                }
                Ok(())
            }
            Some(e) => {
                let ce = self.expr(e, None)?;
                let base = self.exc_base_ty();
                match &ce.ty {
                    Ty::Type(c) => {
                        let c = *c;
                        let call = self.construct(c, vec![], vec![], span)?;
                        self.emit(&format!("fpy_raise((FpyObj*){});", call.c));
                    }
                    Ty::Class(_) if self.assignable(&ce.ty, &base) => self.emit(&format!("fpy_raise((FpyObj*){});", ce.c)),
                    Ty::Any => self.emit(&format!("fpy_raise(fpy_exc_instance({}));", ce.c)),
                    t => return Err(self.e(span, format!("exceptions must derive from BaseException, not {}", self.tyname(t)))),
                }
                Ok(())
            }
        }
    }

    /// Condition matching the current exception against an except clause, plus the bound type.
    fn handler_match(&mut self, t: &Expr, exv: &str) -> R<(String, Ty)> {
        let items: Vec<Expr> = match &t.kind {
            ExprKind::Tuple { elts, .. } => elts.clone(),
            _ => vec![t.clone()],
        };
        let m = self.cur_mod();
        let subst = self.fxr().subst.clone();
        let mut conds = Vec::new();
        let mut natives = Vec::new();
        let mut dynamic = false;
        for x in &items {
            match self.resolve_ann(x, m, &subst) {
                Ok(Ty::Class(c)) => {
                    conds.push(format!("fpy_exc_match({}, &T_{})", exv, self.classes[c].cname));
                    natives.push(c);
                }
                _ => {
                    // a Python exception class (Any): match at runtime
                    let v = self.expr(x, None)?;
                    if v.ty != Ty::Any {
                        return Err(self.e(x.span, "except clause must name exception classes"));
                    }
                    conds.push(format!("fpy_exc_match_dyn({}, {})", exv, v.c));
                    dynamic = true;
                }
            }
        }
        let ty = if dynamic || natives.is_empty() { Ty::Any } else { Ty::Class(self.common_base(&natives)) };
        Ok((conds.join(" || "), ty))
    }

    pub fn resolve_exc_type(&mut self, e: &Expr, m: usize) -> R<Ty> {
        let subst = self.fx_stack.last().map(|f| f.subst.clone()).unwrap_or_default();
        match &e.kind {
            ExprKind::Tuple { elts, .. } => {
                let mut cs = Vec::new();
                for x in elts {
                    match self.resolve_ann(x, m, &subst)? {
                        Ty::Class(c) => cs.push(c),
                        _ => return Err(self.err(m, x.span, "except clause must name exception classes")),
                    }
                }
                Ok(Ty::Class(self.common_base(&cs)))
            }
            _ => match self.resolve_ann(e, m, &subst)? {
                Ty::Class(c) => Ok(Ty::Class(c)),
                _ => Err(self.err(m, e.span, "except clause must name an exception class")),
            },
        }
    }

    pub fn common_base(&self, cs: &[ClassId]) -> ClassId {
        let mut cand = cs[0];
        loop {
            if cs.iter().all(|c| self.is_subclass(*c, cand)) {
                return cand;
            }
            match self.classes[cand].base {
                Some(b) => cand = b,
                None => return cand,
            }
        }
    }

    fn try_stmt(&mut self, body: &[Stmt], handlers: &[ExceptHandler], orelse: &[Stmt], finalbody: &[Stmt]) -> R<()> {
        let has_finally = !finalbody.is_empty();
        let ff = self.fresh("ff");
        let pend = self.fresh("pend");
        if has_finally {
            self.decl(format!("FpyExcFrame {};", ff));
            self.decl(format!("FpyObj * volatile {} = NULL;", pend));
            self.emit(&format!("{} = NULL;", pend));
            self.emit(&format!("if (FPY_TRY({})) {{", ff));
            self.fx().indent += 1;
            self.fx().cleanup.push(Cleanup::Frame(ff.clone()));
            self.fx().cleanup.push(Cleanup::Finally(finalbody.to_vec()));
        }
        if handlers.is_empty() {
            self.block(body)?;
            self.block(orelse)?;
        } else {
            let ef = self.fresh("ef");
            self.decl(format!("FpyExcFrame {};", ef));
            self.emit(&format!("if (FPY_TRY({})) {{", ef));
            self.fx().indent += 1;
            self.fx().cleanup.push(Cleanup::Frame(ef.clone()));
            self.block(body)?;
            self.fx().cleanup.pop();
            self.emit(&format!("FPY_POP({});", ef));
            self.block(orelse)?;
            self.fx().indent -= 1;
            self.emit("} else {");
            self.fx().indent += 1;
            self.emit(&format!("FPY_POP({});", ef));
            let exv = self.fresh("exc");
            self.decl(format!("FpyObj *{} = NULL;", exv));
            self.emit(&format!("{} = fpy_cur_exc;", exv));
            let mut first = true;
            let mut has_catch_all = false;
            for h in handlers {
                let (cond, ety) = match &h.typ {
                    None => {
                        has_catch_all = true;
                        ("1".to_string(), Ty::Any)
                    }
                    Some(t) => self.handler_match(t, &exv)?,
                };
                self.emit(&format!("{}if ({}) {{", if first { "" } else { "else " }, cond));
                first = false;
                self.fx().indent += 1;
                // `except E as name` binds a handler-scoped variable (Python unbinds it afterwards).
                let scoped = h.name.is_some();
                if let Some(n) = &h.name {
                    let ct = self.ctype(&ety);
                    let cname = self.fresh(&format!("e_{}", c_ident(n)));
                    self.decl(format!("{} {} = NULL;", ct, cname));
                    match &ety {
                        Ty::Class(c) => {
                            let tn = self.classes[*c].cname.clone();
                            self.emit(&format!("{} = (({})fpy_exc_as({}, &T_{}));", cname, ct, exv, tn));
                        }
                        _ => self.emit(&format!("{} = (({}){});", cname, ct, exv)),
                    }
                    self.fx().comp.push(HashMap::from([(n.clone(), Local { ty: ety.clone(), cname, cell: false })]));
                }
                self.fx().handler_exc.push(exv.clone());
                let r = self.block(&h.body);
                self.fx().handler_exc.pop();
                if scoped {
                    self.fx().comp.pop();
                }
                r?;
                self.fx().indent -= 1;
                self.emit("}");
            }
            if !has_catch_all {
                self.emit(&format!("else fpy_raise({});", exv));
            }
            self.fx().indent -= 1;
            self.emit("}");
        }
        if has_finally {
            self.fx().cleanup.pop();
            self.fx().cleanup.pop();
            self.emit(&format!("FPY_POP({});", ff));
            self.fx().indent -= 1;
            self.emit(&format!("}} else {{ FPY_POP({}); {} = fpy_cur_exc; }}", ff, pend));
            self.block(finalbody)?;
            self.emit(&format!("if ({}) fpy_raise({});", pend, pend));
        }
        Ok(())
    }

    pub fn enter_type(&mut self, t: &Ty, span: Span) -> R<Ty> {
        match t {
            Ty::Class(c) => {
                let f = self.find_method(*c, "__enter__").ok_or_else(|| self.e(span, format!("'{}' is not a context manager (no __enter__)", self.classes[*c].name)))?;
                Ok(self.func_sig(f)?.ret)
            }
            Ty::Any => Ok(Ty::Any),
            _ => Err(self.e(span, format!("{} is not a context manager", self.tyname(t)))),
        }
    }

    fn with_stmt(&mut self, items: &[WithItem], body: &[Stmt], span: Span, is_async: bool) -> R<()> {
        if items.len() > 1 {
            let inner = Stmt { kind: StmtKind::With { items: items[1..].to_vec(), body: body.to_vec(), is_async }, span, end_line: span.line, header_end: span.line };
            return self.with_stmt(&items[..1], &[inner], span, is_async);
        }
        let it = &items[0];
        let mgr = self.expr(&it.ctx, None)?;
        let mt = self.tmp_var(&mgr.ty, "mgr");
        self.emit(&format!("{} = {};", mt, mgr.c));
        let mname = format!("\u{1}{}", mt);
        self.fx().comp.push(HashMap::from([(mname.clone(), Local { ty: mgr.ty.clone(), cname: mt.clone(), cell: false })]));
        let mexpr = Expr::new(ExprKind::Name(mname), it.ctx.span);
        let call = |name: &str, args: Vec<Expr>| {
            let (n, wrap) = match (name, is_async) {
                ("__enter__", true) => ("__aenter__", true),
                ("__exit__", true) => ("__aexit__", true),
                (n, _) => (n, false),
            };
            let c = Expr::new(ExprKind::Call { func: Box::new(Expr::new(ExprKind::Attribute { value: Box::new(mexpr.clone()), attr: n.into() }, span)), args, keywords: vec![] }, span);
            if wrap {
                Expr::new(ExprKind::Await(Box::new(c)), span)
            } else {
                c
            }
        };
        let enter = call("__enter__", vec![]);
        let none = Expr::new(ExprKind::NoneLit, span);
        let exit_ok = Stmt { kind: StmtKind::Expr(call("__exit__", vec![none.clone(), none.clone(), none.clone()])), span, end_line: span.line, header_end: span.line };
        let r = (|| -> R<()> {
            let ev = self.expr(&enter, None)?;
            match &it.var {
                Some(v) => self.store_target(v, ev)?,
                None => self.emit(&format!("(void)({});", ev.c)),
            }
            // body runs under an exception frame; normal exits (incl. return/break) call __exit__(None, None, None)
            let wf = self.fresh("wf");
            self.decl(format!("FpyExcFrame {};", wf));
            self.emit(&format!("if (FPY_TRY({})) {{", wf));
            self.fx().indent += 1;
            self.fx().cleanup.push(Cleanup::Frame(wf.clone()));
            self.fx().cleanup.push(Cleanup::Finally(vec![exit_ok.clone()]));
            self.block(body)?;
            self.fx().cleanup.pop();
            self.fx().cleanup.pop();
            self.emit(&format!("FPY_POP({});", wf));
            self.block(std::slice::from_ref(&exit_ok))?;
            self.fx().indent -= 1;
            self.emit("} else {");
            self.fx().indent += 1;
            self.emit(&format!("FPY_POP({});", wf));
            // exceptional exit: __exit__(type, value, None); a truthy result suppresses the exception
            let ex = self.fresh("wexc");
            self.decl(format!("FpyObj *{} = NULL;", ex));
            self.emit(&format!("{} = fpy_cur_exc;", ex));
            let en = format!("\u{1}{}", ex);
            self.fx().comp.push(HashMap::from([(en.clone(), Local { ty: Ty::Any, cname: format!("({})", ex), cell: false })]));
            let ev_expr = Expr::new(ExprKind::Name(en.clone()), span);
            let ty_expr = Expr::new(ExprKind::Name(format!("{}_type", en)), span);
            self.fx().comp.last_mut().unwrap().insert(format!("{}_type", en), Local { ty: Ty::Any, cname: format!("fpy_exc_type_of({})", ex), cell: false });
            let exit_err = call("__exit__", vec![ty_expr, ev_expr, none.clone()]);
            let res = self.expr(&exit_err, None);
            self.fx().comp.pop();
            let res = res?;
            let suppress = match &res.ty {
                Ty::NoneT => "false".to_string(),
                _ => {
                    let t = self.truthy(&res)?;
                    t
                }
            };
            if res.ty == Ty::NoneT {
                self.emit(&format!("{};", res.c));
                self.emit(&format!("fpy_raise({});", ex));
            } else {
                self.emit(&format!("if (!({})) fpy_raise({});", suppress, ex));
            }
            self.fx().indent -= 1;
            self.emit("}");
            Ok(())
        })();
        self.fx().comp.pop();
        r
    }

    fn nested_function(&mut self, fd: &FuncDef, span: Span) -> R<()> {
        if !fd.type_params.is_empty() {
            return Err(self.e(span, "nested generic functions are not supported"));
        }
        let m = self.cur_mod();
        let subst = self.fxr().subst.clone();
        let class = self.fxr().class;
        // signature
        let fake = Rc::new(fd.clone());
        let fid = self.new_func(m, fake, None, FnKind::Plain, subst.clone(), String::new());
        let sig = self.func_sig(fid)?;
        // the value's type follows the C signature: params, with *args (a list) before the
        // keyword-only ones and **kwargs (a dict) last
        let mut ptys: Vec<Ty> = sig.params.iter().map(|p| p.ty.clone()).collect();
        if let Some((_, vt)) = &sig.vararg {
            let pos = sig.params.iter().position(|p| p.kwonly).unwrap_or(sig.params.len());
            ptys.insert(pos, vt.clone());
        }
        if let Some((_, kt)) = &sig.kwarg {
            ptys.push(kt.clone());
        }
        let ftype = Ty::Func(ptys, Box::new(sig.ret.clone()));
        self.fx().nested_sigs.insert(fd.name.clone(), sig.clone());
        // Make the name visible (as a cell if recursive) before compiling the body.
        if !self.fxr().locals.contains_key(&fd.name) {
            self.declare_local(&fd.name, &ftype)?;
        }
        let scope = self.func_scope(fid);
        self.lambda_count += 1;
        let cname = format!("L_{}_{}", c_ident(&fd.name), self.lambda_count);
        let caps: Vec<(String, Ty)> = self.captures_for(&scope, span)?;
        let mut fx = Fx::new(m, class);
        fx.scope = scope.clone();
        fx.ret = sig.ret.clone();
        fx.subst = subst;
        fx.fname = fd.name.clone();
        fx.dry_run = self.fxr().dry_run;
        self.fx_stack.push(fx);
        let res = self.compile_function_body(&cname, fd, &sig, scope.has_yield, Some(&caps));
        self.fx_stack.pop();
        let (proto, code) = res?;
        if !self.fxr().dry_run {
            self.out.protos.push_str(&proto);
            self.out.funcs.push_str(&code);
        }
        let env = self.build_env(&caps)?;
        let fname = c_string(fd.name.as_bytes());
        let dw = if self.fxr().dry_run || scope.has_yield || sig.vararg.is_some() || sig.kwarg.is_some() {
            "NULL".to_string()
        } else {
            self.closure_dyn_wrapper(&cname, &ftype);
            format!("DW_{}", cname)
        };
        let val = CE::new(ftype, format!("fpy_func_new((void*){}, {}, {}, {})", cname, env, dw, fname));
        self.store_name(&fd.name, val, span)
    }

    /// Captured variables (with types) for a nested scope.
    pub fn captures_for(&mut self, scope: &Scope, span: Span) -> R<Vec<(String, Ty)>> {
        let mut caps = Vec::new();
        for f in scope.frees.iter().chain(scope.nonlocals.iter()) {
            if caps.iter().any(|(n, _): &(String, Ty)| n == f) {
                continue;
            }
            if let Some(l) = self.fxr().locals.get(f) {
                if !l.cell {
                    return Err(self.e(span, format!("internal: captured variable '{}' is not a cell", f)));
                }
                caps.push((f.clone(), l.ty.clone()));
            } else if let Some((_, t)) = self.fxr().env.get(f) {
                caps.push((f.clone(), t.clone()));
            } else if self.fxr().scope.locals.contains(f) {
                return Err(self.e(span, format!("variable '{}' is captured before it is assigned; assign it first", f)));
            } else {
                for c in self.fxr().comp.iter() {
                    if c.contains_key(f) {
                        return Err(self.e(span, format!("closures cannot capture comprehension variable '{}'", f)));
                    }
                }
            }
        }
        Ok(caps)
    }

    pub fn build_env(&mut self, caps: &[(String, Ty)]) -> R<String> {
        if caps.is_empty() {
            return Ok("NULL".into());
        }
        let kinds = vec![FK_OBJ; caps.len()];
        let karr = self.kinds_array(&kinds);
        let mut s = format!("({{ FpyTuple *_env = fpy_tuple_new({}, {}); ", caps.len(), karr);
        for (i, (n, _)) in caps.iter().enumerate() {
            let cell = if let Some(l) = self.fxr().locals.get(n) {
                l.cname.clone()
            } else {
                let (slot, _) = self.fxr().env[n];
                format!("((FpyCell*)fpy_pw(env->items[{}]))", slot)
            };
            s.push_str(&format!("_env->items[{}] = fpy_wp({}); ", i, cell));
        }
        s.push_str("_env; })");
        Ok(s)
    }

    /// `async for x in it:` == `_ai = it.__aiter__(); while True: try: x = await _ai.__anext__() except StopAsyncIteration: break; body`
    fn async_for(&mut self, target: &Expr, iter: &Expr, body: &[Stmt], span: Span) -> R<()> {
        let it = self.expr(iter, None)?;
        let ai = self.tmp_var(&Ty::Any, "ai");
        let itc = self.coerce(&it.c, &it.ty, &Ty::Any);
        self.emit(&format!("{} = fpy_dyn_call_method({}, \"__aiter__\", NULL, 0);", ai, itc));
        let name = format!("\u{1}{}", ai);
        self.fx().comp.push(HashMap::from([(name.clone(), Local { ty: Ty::Any, cname: ai.clone(), cell: false })]));
        let anext = Expr::new(
            ExprKind::Await(Box::new(Expr::new(ExprKind::Call { func: Box::new(Expr::new(ExprKind::Attribute { value: Box::new(Expr::new(ExprKind::Name(name), span)), attr: "__anext__".into() }, span)), args: vec![], keywords: vec![] }, span))),
            span,
        );
        let mk = |kind: StmtKind| Stmt { kind, span, end_line: span.line, header_end: span.line };
        let try_stmt = mk(StmtKind::Try {
            body: vec![mk(StmtKind::Assign { targets: vec![target.clone()], value: anext })],
            handlers: vec![ExceptHandler { typ: Some(Expr::new(ExprKind::Name("StopAsyncIteration".into()), span)), name: None, body: vec![mk(StmtKind::Break)], span, header_end: span.line }],
            orelse: vec![],
            finalbody: vec![],
            star: false,
        });
        let mut wbody = vec![try_stmt];
        wbody.extend(body.iter().cloned());
        let w = mk(StmtKind::While { test: Expr::new(ExprKind::Bool(true), span), body: wbody, orelse: vec![] });
        let r = self.stmt(&w);
        self.fx().comp.pop();
        r
    }

    // ------------------------------------------------------------------ Python interop

    fn src_slice(&self, m: usize, e: &Expr) -> String {
        let src = &self.modules[m].src;
        src.get(e.span.start..e.span.end.min(src.len())).unwrap_or("").to_string()
    }

    /// C expression creating a Python callable (with signature metadata) for a native function.
    pub fn py_native_value(&mut self, fid: usize) -> R<String> {
        self.uses_python = true;
        let def = self.funcs[fid].def.clone();
        let m = self.funcs[fid].module;
        self.func_sig(fid)?;
        let dw = self.dyn_wrapper(fid)?;
        let fv = CE::new(Ty::Any, format!("fpy_func_new(NULL, NULL, {}, {})", dw, c_string(def.name.as_bytes())));
        let mut names = Vec::new();
        let mut anns = Vec::new();
        let mut defs = Vec::new();
        for a in def.params.all_positional().chain(def.params.kwonly.iter()) {
            names.push(c_string(a.name.as_bytes()));
            anns.push(match &a.ann {
                Some(x) => c_string(self.src_slice(m, x).as_bytes()),
                None => "NULL".into(),
            });
            defs.push(match &a.default {
                Some(x) => c_string(self.src_slice(m, x).as_bytes()),
                None => "NULL".into(),
            });
        }
        if let Some(v) = &def.params.vararg {
            names.push(c_string(format!("*{}", v.name).as_bytes()));
            anns.push("NULL".into());
            defs.push("NULL".into());
        }
        let ret = match &def.returns {
            Some(r) => c_string(self.src_slice(m, r).as_bytes()),
            None => "NULL".into(),
        };
        let modname = if self.modules[m].name == "__main__" { "__main__".to_string() } else { self.modules[m].name.clone() };
        let n = names.len();
        let arr = |v: &Vec<String>| if v.is_empty() { "NULL".to_string() } else { format!("(const char*[]){{{}}}", v.join(", ")) };
        let doc = match def.body.first().map(|s| &s.kind) {
            Some(StmtKind::Expr(Expr { kind: ExprKind::Str { value, .. }, .. })) => c_string(value.as_bytes()),
            _ => "NULL".into(),
        };
        Ok(format!(
            "fpy_py_native((FpyFunc*)({}), {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            fv.c,
            c_string(def.name.as_bytes()),
            c_string(self.funcs[fid].qualname.as_bytes()),
            c_string(modname.as_bytes()),
            n,
            arr(&names),
            arr(&anns),
            arr(&defs),
            ret,
            if def.is_async { 1 } else { 0 },
            doc
        ))
    }

    /// Decorators whose value is a Python object (e.g. `@app.get("/")`) are applied at definition time.
    fn apply_dynamic_decorators(&mut self, fid: usize, fd: &FuncDef) -> R<()> {
        let known = ["staticmethod", "classmethod", "property", "extern", "overload", "dataclass", "abstractmethod", "override", "final"];
        let mut dyn_decos = Vec::new();
        for d in &fd.decorators {
            let name = match &d.kind {
                ExprKind::Name(n) => Some(n.clone()),
                ExprKind::Call { func, .. } => match &func.kind {
                    ExprKind::Name(n) => Some(n.clone()),
                    _ => None,
                },
                _ => None,
            };
            if name.as_deref().map(|n| known.contains(&n)).unwrap_or(false) {
                continue;
            }
            dyn_decos.push(d.clone());
        }
        if dyn_decos.is_empty() {
            return Ok(());
        }
        let native = self.py_native_value(fid)?;
        let fv = self.fresh("deco");
        self.decl(format!("FpyObj *{} = NULL;", fv));
        self.emit(&format!("{} = {};", fv, native));

        for d in dyn_decos.iter().rev() {
            let dv = self.expr(d, None)?;
            if dv.ty != Ty::Any {
                return Err(self.e(d.span, format!("decorator of type {} is not supported on native functions", self.tyname(&dv.ty))));
            }
            self.emit(&format!("{} = fpy_dyn_call({}, &{}, 1);", fv, dv.c, fv));
        }
        let modname = self.modules[self.cur_mod()].name.clone();
        if modname == "__main__" {
            // keep the decorated object reachable from Python (e.g. routes, tools)
            self.emit(&format!("fpy_py_publish({}, {});", c_string(fd.name.as_bytes()), fv));
        }
        Ok(())
    }

    /// Create a Python class (deriving from Python bases) with native methods.
    fn emit_py_class(&mut self, pi: usize) -> R<()> {
        self.uses_python = true;
        let def = self.py_classes[pi].def.clone();
        let m = self.py_classes[pi].module;
        let gid = self.py_classes[pi].global;
        let methods = self.py_classes[pi].methods.clone();
        // bases
        let mut bases = Vec::new();
        for b in &def.bases {
            let v = self.expr(b, None)?;
            bases.push(self.coerce(&v.c, &v.ty, &Ty::Any));
        }
        let blist = format!("({{ FpyList *_b = fpy_list_new(FK_OBJ, {}); {} _b; }})", bases.len(), bases.iter().map(|b| format!("fpy_list_append(_b, fpy_wp({}));", b)).collect::<Vec<_>>().join(" "));
        // methods
        let mut mcode = String::from("({ FpyDict *_m = fpy_dict_new(FK_OBJ, FK_OBJ); ");
        for (name, fid, wrap) in &methods {
            self.func_sig(*fid)?;
            self.func_instance(*fid, vec![])?;
            let mut v = self.py_native_value(*fid)?;
            if let Some(w) = wrap {
                v = format!("fpy_py_builtin_wrap({}, {})", c_string(w.as_bytes()), v);
            }
            let key = self.string_lit(name);
            mcode.push_str(&format!("fpy_dict_set(_m, fpy_wp({}), fpy_wp({})); ", key, v));
        }
        mcode.push_str("_m; })");
        // annotations and defaults from the class body, as Python source
        let mut anns = Vec::new();
        let mut defaults = Vec::new();
        for st in &def.body {
            match &st.kind {
                StmtKind::AnnAssign { target, ann, value } => {
                    if let ExprKind::Name(n) = &target.kind {
                        anns.push((n.clone(), self.src_slice(m, ann)));
                        if let Some(v) = value {
                            defaults.push((n.clone(), self.src_slice(m, v)));
                        }
                    }
                }
                StmtKind::Assign { targets, value } => {
                    for t in targets {
                        if let ExprKind::Name(n) = &t.kind {
                            defaults.push((n.clone(), self.src_slice(m, value)));
                        }
                    }
                }
                _ => {}
            }
        }
        let pairs = |c: &mut Compiler, v: &Vec<(String, String)>| -> String {
            let mut code = format!("({{ FpyList *_p = fpy_list_new(FK_OBJ, {}); ", v.len());
            let karr = c.kinds_array(&[FK_OBJ, FK_OBJ]);
            for (a, b) in v {
                let sa = c.string_lit(a);
                let sb = c.string_lit(b);
                code.push_str(&format!("{{ FpyTuple *_t = fpy_tuple_new(2, {}); _t->items[0] = fpy_wp({}); _t->items[1] = fpy_wp({}); fpy_list_append(_p, fpy_wp(_t)); }} ", karr, sa, sb));
            }
            code.push_str("_p; })");
            code
        };
        let acode = pairs(self, &anns);
        let dcode = pairs(self, &defaults);
        let modname = if self.modules[m].name == "__main__" { "__main__".to_string() } else { self.modules[m].name.clone() };
        let gn = self.globals[gid].cname.clone();
        self.emit(&format!(
            "G.{} = fpy_py_make_class({}, {}, (FpyObj*){}, (FpyObj*){}, (FpyObj*){}, (FpyObj*){});",
            gn,
            c_string(def.name.as_bytes()),
            c_string(modname.as_bytes()),
            blist,
            mcode,
            acode,
            dcode
        ));
        self.emit(&format!("fpy_py_publish({}, G.{});", c_string(def.name.as_bytes()), gn));
        // dynamic class decorators
        for d in def.decorators.iter().rev() {
            let dv = self.expr(d, None)?;
            if dv.ty == Ty::Any {
                self.emit(&format!("G.{} = fpy_dyn_call({}, &G.{}, 1);", gn, dv.c, gn));
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ for loops

    pub fn elem_type(&mut self, t: &Ty) -> Option<Ty> {
        match t {
            Ty::List(e) | Ty::Set(e) | Ty::Iter(e) | Ty::VarTuple(e) => Some((**e).clone()),
            Ty::Dict(k, _) => Some((**k).clone()),
            Ty::Str => Some(Ty::Str),
            Ty::Bytes => Some(Ty::Int),
            Ty::Tuple(ts) if !ts.is_empty() && ts.iter().all(|x| x == &ts[0]) => Some(ts[0].clone()),
            Ty::Any => Some(Ty::Any),
            Ty::Class(c) => {
                let f = self.find_method(*c, "__iter__")?;
                match self.func_sig(f).ok()?.ret {
                    Ty::Iter(e) => Some(*e),
                    Ty::Class(it) => {
                        let n = self.find_method(it, "__next__")?;
                        Some(self.func_sig(n).ok()?.ret)
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Emit a loop binding `target` to each element of `iter`, running `body` each time.
    pub fn for_loop(&mut self, target: &Expr, iter: &Expr, broke: Option<String>, body: &mut dyn FnMut(&mut Compiler) -> R<()>) -> R<()> {
        let depth = self.fxr().cleanup.len();
        // range()
        if let ExprKind::Call { func, args, keywords } = &iter.kind {
            if let ExprKind::Name(n) = &func.kind {
                if n == "range" && keywords.is_empty() && !args.is_empty() && args.len() <= 3 && self.is_builtin_name(n) {
                    let mut cs = Vec::new();
                    for a in args {
                        let ce = self.expr(a, Some(&Ty::Int))?;
                        cs.push(self.check_coerce(&ce, &Ty::Int, a.span)?);
                    }
                    let (start, stop, step) = match cs.len() {
                        1 => ("0".to_string(), cs[0].clone(), "1".to_string()),
                        2 => (cs[0].clone(), cs[1].clone(), "1".to_string()),
                        _ => (cs[0].clone(), cs[1].clone(), cs[2].clone()),
                    };
                    let i = self.fresh("i");
                    let e = self.fresh("e");
                    let s = self.fresh("s");
                    let step_lit: Option<i64> = match args.get(2).map(|a| &a.kind) {
                        None => Some(1),
                        Some(ExprKind::Int(v)) => Some(*v),
                        Some(ExprKind::UnaryOp { op: UnOp::Neg, operand }) => match operand.kind {
                            ExprKind::Int(v) => Some(-v),
                            _ => None,
                        },
                        _ => None,
                    };
                    let cmp = match step_lit {
                        Some(v) if v > 0 => format!("{} < {}", i, e),
                        Some(v) if v < 0 => format!("{} > {}", i, e),
                        _ => format!("({s} > 0 ? {i} < {e} : {i} > {e})"),
                    };
                    if step_lit == Some(0) {
                        return Err(self.e(iter.span, "range() step must not be zero"));
                    }
                    if step_lit.is_none() {
                        self.emit(&format!("if (({}) == 0) fpy_raise_code(FPY_EXC_VALUE, \"range() arg 3 must not be zero\");", step));
                    }
                    self.emit(&format!("for (fpy_int {i} = {start}, {e} = {stop}, {s} = {step}; {cmp}; {i} += {s}) {{"));
                    self.emit(&format!("    (void){};", s));
                    return self.loop_body(target, CE::new(Ty::Int, i), depth, broke, body);
                }
                if n == "enumerate" && self.is_builtin_name(n) && args.len() <= 2 {
                    if let ExprKind::Tuple { elts, .. } = &target.kind {
                        if elts.len() == 2 {
                            let start = match args.get(1) {
                                Some(a) => {
                                    let ce = self.expr(a, Some(&Ty::Int))?;
                                    self.check_coerce(&ce, &Ty::Int, a.span)?
                                }
                                None => match keywords.iter().find(|k| k.name.as_deref() == Some("start")) {
                                    Some(k) => {
                                        let ce = self.expr(&k.value, Some(&Ty::Int))?;
                                        self.check_coerce(&ce, &Ty::Int, k.value.span)?
                                    }
                                    None => "0".into(),
                                },
                            };
                            let counter = self.tmp_var(&Ty::Int, "n");
                            self.emit(&format!("{} = {};", counter, start));
                            let idx_t = elts[0].clone();
                            let val_t = elts[1].clone();
                            let c2 = counter.clone();
                            return self.for_loop(&val_t, &args[0], broke, &mut |c: &mut Compiler| {
                                // increment before the body so `continue` keeps the count right
                                c.store_target(&idx_t, CE::new(Ty::Int, format!("{}++", c2)))?;
                                body(c)
                            });
                        }
                    }
                }
                if n == "zip" && self.is_builtin_name(n) && args.len() >= 2 && keywords.is_empty() {
                    if let ExprKind::Tuple { elts, .. } = &target.kind {
                        if elts.len() == args.len() {
                            return self.zip_loop(elts, args, depth, broke, body);
                        }
                    }
                }
                if n == "reversed" && self.is_builtin_name(n) && args.len() == 1 {
                    let ce = self.expr(&args[0], None)?;
                    if let Ty::List(et) = &ce.ty {
                        let et = (**et).clone();
                        let l = self.tmp_var(&ce.ty, "l");
                        self.emit(&format!("{} = {};", l, ce.c));
                        let k = self.fresh("k");
                        self.emit(&format!("for (int64_t {k} = {l}->len - 1; {k} >= 0; {k}--) {{"));
                        self.emit(&format!("    if ({k} >= {l}->len) continue;"));
                        let ct = self.ctype(&et);
                        let item = et.from_word(&format!("{}->items[{}]", l, k), &ct);
                        return self.loop_body(target, CE::new(et, item), depth, broke, body);
                    }
                }
            }
            // dict.items()/keys()/values()
            if let ExprKind::Attribute { value, attr } = &func.kind {
                if args.is_empty() && matches!(attr.as_str(), "items" | "keys" | "values") {
                    let d = self.expr(value, None)?;
                    if let Ty::Dict(kt, vt) = d.ty.clone() {
                        let dv = self.tmp_var(&d.ty, "d");
                        self.emit(&format!("{} = {};", dv, d.c));
                        let pos = self.fresh("p");
                        let ent = self.fresh("en");
                        let ln = self.fresh("ln");
                        self.emit(&format!("for (int64_t {pos} = 0, {ln} = {dv}->len; ; ) {{"));
                        self.emit(&format!("    FpyDictEntry *{ent};"));
                        self.emit(&format!("    if ({dv}->len != {ln}) fpy_raise_code(FPY_EXC_RUNTIME, \"dictionary changed size during iteration\");"));
                        self.emit(&format!("    if (!fpy_dict_next({dv}, &{pos}, &{ent})) break;"));
                        let kct = self.ctype(&kt);
                        let vct = self.ctype(&vt);
                        let kc = kt.from_word(&format!("{}->key", ent), &kct);
                        let vc = vt.from_word(&format!("{}->value", ent), &vct);
                        let item = match attr.as_str() {
                            "keys" => CE::new((*kt).clone(), kc),
                            "values" => CE::new((*vt).clone(), vc),
                            _ => {
                                if let ExprKind::Tuple { elts, .. } = &target.kind {
                                    if elts.len() == 2 {
                                        let (a, b) = (elts[0].clone(), elts[1].clone());
                                        let (kt2, vt2) = ((*kt).clone(), (*vt).clone());
                                        return self.loop_body_with(depth, broke, &mut |c: &mut Compiler| {
                                            c.store_target(&a, CE::new(kt2.clone(), kc.clone()))?;
                                            c.store_target(&b, CE::new(vt2.clone(), vc.clone()))?;
                                            body(c)
                                        });
                                    }
                                }
                                let karr = self.kinds_array(&[kt.kind(), vt.kind()]);
                                let tc = format!("({{ FpyTuple *_t = fpy_tuple_new(2, {}); _t->items[0] = {}->key; _t->items[1] = {}->value; _t; }})", karr, ent, ent);
                                CE::new(Ty::Tuple(vec![(*kt).clone(), (*vt).clone()]), tc)
                            }
                        };
                        return self.loop_body(target, item, depth, broke, body);
                    }
                }
            }
        }
        let it = self.expr(iter, None)?;
        match it.ty.clone() {
            Ty::List(et) => {
                let l = self.tmp_var(&it.ty, "l");
                self.emit(&format!("{} = {};", l, it.c));
                let k = self.fresh("k");
                self.emit(&format!("for (int64_t {k} = 0; {k} < {l}->len; {k}++) {{"));
                let ct = self.ctype(&et);
                let item = et.from_word(&format!("{}->items[{}]", l, k), &ct);
                self.loop_body(target, CE::new((*et).clone(), item), depth, broke, body)
            }
            Ty::Tuple(ts) if !ts.is_empty() && ts.iter().all(|x| x == &ts[0]) => {
                let t = self.tmp_var(&it.ty, "tp");
                self.emit(&format!("{} = {};", t, it.c));
                let k = self.fresh("k");
                self.emit(&format!("for (int64_t {k} = 0; {k} < {t}->len; {k}++) {{"));
                let ct = self.ctype(&ts[0]);
                let item = ts[0].from_word(&format!("{}->items[{}]", t, k), &ct);
                self.loop_body(target, CE::new(ts[0].clone(), item), depth, broke, body)
            }
            Ty::Class(c) if self.find_method(c, "__iter__").is_some() => {
                let f = self.find_method(c, "__iter__").unwrap();
                let ret = self.func_sig(f)?.ret;
                match ret {
                    Ty::Class(itc) => {
                        // user iterator protocol: __next__ raising StopIteration
                        let nf = self.find_method(itc, "__next__").ok_or_else(|| self.e(iter.span, "__iter__ must return an object with __next__"))?;
                        let et = self.func_sig(nf)?.ret;
                        let iterobj = self.call_method_ce(it, f, vec![], None, iter.span)?;
                        let iv = self.tmp_var(&iterobj.ty, "it");
                        self.emit(&format!("{} = {};", iv, iterobj.c));
                        let nx = self.call_method_ce(CE::new(iterobj.ty.clone(), iv.clone()), nf, vec![], None, iter.span)?;
                        let xv = self.tmp_var(&et, "x");
                        let fr = self.fresh("sf");
                        let si = self.builtin_class("StopIteration").unwrap();
                        let sic = self.class_cname(si);
                        self.decl(format!("FpyExcFrame {};", fr));
                        self.emit("for (;;) {");
                        self.emit(&format!("    if (FPY_TRY({fr})) {{ {xv} = {}; FPY_POP({fr}); }}", nx.c));
                        self.emit(&format!("    else {{ FPY_POP({fr}); if (fpy_isinstance_t(fpy_cur_exc, &T_{sic})) break; fpy_raise(fpy_cur_exc); }}"));
                        self.loop_body(target, CE::new(et, xv), depth, broke, body)
                    }
                    _ => {
                        let ic = self.coerce(&it.c, &it.ty, &ret);
                        self.iter_loop(target, CE::new(ret, ic), depth, broke, body, iter.span)
                    }
                }
            }
            Ty::Set(_) | Ty::Dict(..) | Ty::Str | Ty::Iter(_) | Ty::Any | Ty::Bytes | Ty::Tuple(_) | Ty::VarTuple(_) => self.iter_loop(target, it, depth, broke, body, iter.span),
            t => Err(self.e(iter.span, format!("{} is not iterable", self.tyname(&t)))),
        }
    }

    fn iter_loop(&mut self, target: &Expr, it: CE, depth: usize, broke: Option<String>, body: &mut dyn FnMut(&mut Compiler) -> R<()>, span: Span) -> R<()> {
        let et = self.elem_type(&it.ty).ok_or_else(|| self.e(span, format!("{} is not iterable", self.tyname(&it.ty))))?;
        let ic = match &it.ty {
            Ty::Iter(_) => it.c.clone(),
            Ty::Any => format!("fpy_dyn_iter({})", it.c),
            Ty::Bytes => {
                let kinds = format!("({{ FpyBytes *_b = {}; FpyList *_l = fpy_list_new(FK_INT, _b->len); for (int64_t _i = 0; _i < _b->len; _i++) fpy_list_append(_l, _b->data[_i]); fpy_list_iter(_l); }})", it.c);
                kinds
            }
            Ty::Tuple(_) => {
                return Err(self.e(span, "iterating a heterogeneous tuple is not supported"));
            }
            _ => self.coerce(&it.c, &it.ty, &Ty::iter(et.clone())),
        };
        let iv = self.tmp_var(&Ty::iter(et.clone()), "it");
        self.emit(&format!("{} = {};", iv, ic));
        let w = self.fresh("w");
        self.emit(&format!("for (fpy_word {w}; {iv}->next({iv}, &{w}); ) {{"));
        let ct = self.ctype(&et);
        let item = et.from_word(&w, &ct);
        self.loop_body(target, CE::new(et, item), depth, broke, body)
    }

    fn zip_loop(&mut self, targets: &[Expr], args: &[Expr], depth: usize, broke: Option<String>, body: &mut dyn FnMut(&mut Compiler) -> R<()>) -> R<()> {
        let mut parts = Vec::new();
        for a in args {
            let ce = self.expr(a, None)?;
            parts.push(ce);
        }
        if parts.iter().all(|p| matches!(p.ty, Ty::List(_))) {
            let mut lists = Vec::new();
            for p in &parts {
                let l = self.tmp_var(&p.ty, "l");
                self.emit(&format!("{} = {};", l, p.c));
                lists.push(l);
            }
            let k = self.fresh("k");
            let conds: Vec<String> = lists.iter().map(|l| format!("{} < {}->len", k, l)).collect();
            self.emit(&format!("for (int64_t {k} = 0; {}; {k}++) {{", conds.join(" && ")));
            let items: Vec<CE> = parts
                .iter()
                .zip(&lists)
                .map(|(p, l)| {
                    let et = match &p.ty {
                        Ty::List(e) => (**e).clone(),
                        _ => unreachable!(),
                    };
                    let ct = self.ctype(&et);
                    CE::new(et.clone(), et.from_word(&format!("{}->items[{}]", l, k), &ct))
                })
                .collect();
            let ts = targets.to_vec();
            return self.loop_body_with(depth, broke, &mut |c: &mut Compiler| {
                for (t, it) in ts.iter().zip(items.iter()) {
                    c.store_target(t, it.clone())?;
                }
                body(c)
            });
        }
        let mut iters = Vec::new();
        let mut ets = Vec::new();
        for (p, a) in parts.iter().zip(args) {
            let et = self.elem_type(&p.ty).ok_or_else(|| self.e(a.span, format!("{} is not iterable", self.tyname(&p.ty))))?;
            let ic = match &p.ty {
                Ty::Any => format!("fpy_dyn_iter({})", p.c),
                _ => self.coerce(&p.c, &p.ty, &Ty::iter(et.clone())),
            };
            let iv = self.tmp_var(&Ty::iter(et.clone()), "it");
            self.emit(&format!("{} = {};", iv, ic));
            iters.push(iv);
            ets.push(et);
        }
        let ws: Vec<String> = iters.iter().map(|_| self.fresh("w")).collect();
        self.emit(&format!("for (fpy_word {};;) {{", ws.join(", ")));
        for (iv, w) in iters.iter().zip(&ws) {
            self.emit(&format!("    if (!{iv}->next({iv}, &{w})) break;"));
        }
        let items: Vec<CE> = ets
            .iter()
            .zip(&ws)
            .map(|(et, w)| {
                let ct = self.ctype(et);
                CE::new(et.clone(), et.from_word(w, &ct))
            })
            .collect();
        let ts = targets.to_vec();
        self.loop_body_with(depth, broke, &mut |c: &mut Compiler| {
            for (t, it) in ts.iter().zip(items.iter()) {
                c.store_target(t, it.clone())?;
            }
            body(c)
        })
    }

    fn loop_body(&mut self, target: &Expr, item: CE, depth: usize, broke: Option<String>, body: &mut dyn FnMut(&mut Compiler) -> R<()>) -> R<()> {
        let t = target.clone();
        self.loop_body_with(depth, broke, &mut |c: &mut Compiler| {
            c.store_target(&t, item.clone())?;
            body(c)
        })
    }

    /// Body of a loop whose header (`for (...) {`) was already emitted.
    fn loop_body_with(&mut self, depth: usize, broke: Option<String>, body: &mut dyn FnMut(&mut Compiler) -> R<()>) -> R<()> {
        self.fx().indent += 1;
        self.fx().loops.push(LoopCtx { cleanup_depth: depth, broke });
        let saved = self.fxr().narrow.len();
        let r = body(self);
        self.fx().narrow.truncate(saved);
        self.fx().loops.pop();
        self.fx().indent -= 1;
        self.emit("}");
        r
    }

    // ------------------------------------------------------------------ match

    fn match_stmt(&mut self, subject: &Expr, cases: &[MatchCase]) -> R<()> {
        let s = self.expr(subject, None)?;
        let sv = self.tmp_var(&s.ty, "subj");
        self.emit(&format!("{} = {};", sv, s.c));
        let subj = CE::new(s.ty.clone(), sv);
        let done = self.fresh("matched");
        self.decl(format!("bool {} = false;", done));
        self.emit(&format!("{} = false;", done));
        for case in cases {
            self.emit(&format!("if (!{}) {{", done));
            self.fx().indent += 1;
            let saved = self.fxr().narrow.len();
            // patterns carry no spans: report their errors at the `case` line
            let at_case = |mut d: Diag| {
                if d.span.line == 0 {
                    d.span = Span { line: case.line, col: 5, start: 0, end: 0 };
                }
                d
            };
            let cond = self.pattern(&case.pattern, &subj).map_err(at_case)?;
            self.emit(&format!("if ({}) {{", cond));
            self.fx().indent += 1;
            self.bind_pattern(&case.pattern, &subj).map_err(at_case)?;
            if let Some(g) = &case.guard {
                let (gc, _) = self.cond(g)?;
                self.emit(&format!("if ({}) {{", gc));
                self.fx().indent += 1;
                self.emit(&format!("{} = true;", done));
                self.block(&case.body)?;
                self.fx().indent -= 1;
                self.emit("}");
            } else {
                self.emit(&format!("{} = true;", done));
                self.block(&case.body)?;
            }
            self.fx().indent -= 1;
            self.emit("}");
            self.fx().narrow.truncate(saved);
            self.fx().indent -= 1;
            self.emit("}");
        }
        Ok(())
    }

    /// Index of sequence-pattern element `i` (of `n`, with an optional star at `star`): elements
    /// after the star count from the end. Returns the C index and, for tuples, the static index.
    fn seq_pattern_index(&self, subj: &CE, n: usize, star: Option<usize>, i: usize) -> (String, usize) {
        match (star, &subj.ty) {
            (Some(s), Ty::Tuple(ts)) if i > s => {
                let k = ts.len() - (n - i);
                (k.to_string(), k)
            }
            (Some(s), _) if i > s => (format!("({})->len - {}", subj.c, n - i), 0),
            _ => (i.to_string(), i),
        }
    }

    /// Returns a C condition testing whether `subj` matches `p` (without binding).
    fn pattern(&mut self, p: &Pattern, subj: &CE) -> R<String> {
        Ok(match p {
            Pattern::Wildcard | Pattern::Capture(_) => "1".into(),
            Pattern::As(inner, _) => self.pattern(inner, subj)?,
            Pattern::Or(ps) => {
                let mut cs = Vec::new();
                for x in ps {
                    cs.push(format!("({})", self.pattern(x, subj)?));
                }
                cs.join(" || ")
            }
            Pattern::Value(e) => {
                let span = e.span;
                let name = format!("\u{1}{}", subj.c);
                self.fx().comp.push(HashMap::from([(name.clone(), Local { ty: subj.ty.clone(), cname: subj.c.clone(), cell: false })]));
                let cmp = Expr::new(ExprKind::Compare { left: Box::new(Expr::new(ExprKind::Name(name), span)), ops: vec![if matches!(e.kind, ExprKind::NoneLit) { CmpOp::Is } else { CmpOp::Eq }], comparators: vec![e.clone()] }, span);
                let r = self.expr(&cmp, Some(&Ty::Bool));
                self.fx().comp.pop();
                r?.c
            }
            Pattern::Class { cls, args, kwargs } => {
                let m = self.cur_mod();
                let subst = self.fxr().subst.clone();
                let ct = self.resolve_ann(cls, m, &subst)?;
                let mut conds = Vec::new();
                let narrowed;
                match (&ct, &subj.ty) {
                    (Ty::Class(c), _) => {
                        let cn = self.classes[*c].cname.clone();
                        conds.push(format!("fpy_isinstance_t((FpyObj*)({}), &T_{})", subj.c, cn));
                        narrowed = CE::new(ct.clone(), format!("((struct {}*)({}))", cn, subj.c));
                    }
                    (Ty::Int, Ty::Int) | (Ty::Str, Ty::Str) | (Ty::Float, Ty::Float) | (Ty::Bool, Ty::Bool) => {
                        narrowed = subj.clone();
                    }
                    (t, Ty::Any) | (t, Ty::Union(_)) | (t, Ty::Optional(_)) => {
                        conds.push(self.isinstance_builtin(&subj.c, t));
                        let un = self.unbox(&subj.c, t);
                        narrowed = CE::new(t.clone(), un);
                    }
                    _ => {
                        conds.push("0".into());
                        narrowed = subj.clone();
                    }
                }
                if let Ty::Class(c) = &ct {
                    let fields = self.match_args(*c);
                    for (i, a) in args.iter().enumerate() {
                        let fname = fields.get(i).cloned().ok_or_else(|| self.e(cls.span, "too many positional sub-patterns"))?;
                        let f = self.find_field(*c, &fname).unwrap();
                        let sub = CE::new(f.ty.clone(), format!("({})->f_{}", narrowed.c, c_ident(&fname)));
                        let pc = self.pattern(a, &sub)?;
                        conds.push(format!("({})", pc));
                    }
                    for (k, a) in kwargs {
                        let f = self.find_field(*c, k).ok_or_else(|| self.e(cls.span, format!("no attribute '{}'", k)))?;
                        let sub = CE::new(f.ty.clone(), format!("({})->f_{}", narrowed.c, c_ident(k)));
                        let pc = self.pattern(a, &sub)?;
                        conds.push(format!("({})", pc));
                    }
                } else if args.len() == 1 {
                    let pc = self.pattern(&args[0], &narrowed)?;
                    conds.push(format!("({})", pc));
                }
                if conds.is_empty() {
                    "1".into()
                } else {
                    conds.join(" && ")
                }
            }
            Pattern::Sequence(ps, _) => {
                let (len, et): (String, Box<dyn Fn(usize) -> Ty>) = match &subj.ty {
                    Ty::List(e) => {
                        let e = (**e).clone();
                        (format!("({})->len", subj.c), Box::new(move |_| e.clone()))
                    }
                    Ty::Tuple(ts) => {
                        let ts = ts.clone();
                        (format!("({})->len", subj.c), Box::new(move |i| ts[i].clone()))
                    }
                    _ => return Err(self.e(Span::default(), "sequence patterns need a list or tuple subject")),
                };
                let star = ps.iter().position(|p| matches!(p, Pattern::Star(_)));
                if let Ty::Tuple(ts) = &subj.ty {
                    // a tuple's length is static: a length mismatch can never match
                    let fits = match star {
                        None => ts.len() == ps.len(),
                        Some(_) => ts.len() + 1 >= ps.len(),
                    };
                    if !fits {
                        return Ok("0".into());
                    }
                }
                let mut conds = vec![match star {
                    None => format!("{} == {}", len, ps.len()),
                    Some(_) => format!("{} >= {}", len, ps.len() - 1),
                }];
                for (i, x) in ps.iter().enumerate() {
                    if matches!(x, Pattern::Star(_)) {
                        continue;
                    }
                    let (idx, ti) = self.seq_pattern_index(subj, ps.len(), star, i);
                    let t = et(ti);
                    let ct = self.ctype(&t);
                    let item = CE::new(t.clone(), t.from_word(&format!("({})->items[{}]", subj.c, idx), &ct));
                    let pc = self.pattern(x, &item)?;
                    if pc != "1" {
                        conds.push(format!("({})", pc));
                    }
                }
                conds.join(" && ")
            }
            Pattern::Mapping { keys, values, .. } => {
                let (kt, vt) = match &subj.ty {
                    Ty::Dict(k, v) => ((**k).clone(), (**v).clone()),
                    _ => return Err(self.e(Span::default(), "mapping patterns need a dict subject")),
                };
                let mut conds = Vec::new();
                for (k, v) in keys.iter().zip(values) {
                    let kc = self.expr(k, Some(&kt))?;
                    let kc = self.check_coerce(&kc, &kt, k.span)?;
                    let kw = kt.to_word(&kc);
                    conds.push(format!("fpy_dict_contains({}, {})", subj.c, kw));
                    let vct = self.ctype(&vt);
                    let item = CE::new(vt.clone(), vt.from_word(&format!("fpy_dict_get({}, {})", subj.c, kw), &vct));
                    let pc = self.pattern(v, &item)?;
                    if pc != "1" {
                        conds.push(format!("({})", pc));
                    }
                }
                if conds.is_empty() {
                    "1".into()
                } else {
                    conds.join(" && ")
                }
            }
            Pattern::Star(_) => "1".into(),
        })
    }

    fn match_args(&self, c: ClassId) -> Vec<String> {
        self.classes[c].fields.iter().map(|f| f.name.clone()).collect()
    }

    fn bind_pattern(&mut self, p: &Pattern, subj: &CE) -> R<()> {
        match p {
            Pattern::Capture(n) => self.store_name(n, subj.clone(), Span::default()),
            Pattern::As(inner, n) => {
                self.bind_pattern(inner, subj)?;
                self.store_name(n, subj.clone(), Span::default())
            }
            Pattern::Class { cls, args, kwargs } => {
                let m = self.cur_mod();
                let subst = self.fxr().subst.clone();
                let ct = self.resolve_ann(cls, m, &subst)?;
                let narrowed = match &ct {
                    Ty::Class(c) => CE::new(ct.clone(), format!("((struct {}*)({}))", self.classes[*c].cname, subj.c)),
                    t if subj.ty.is_boxed() => CE::new(t.clone(), self.unbox(&subj.c, t)),
                    _ => subj.clone(),
                };
                if let Ty::Class(c) = &ct {
                    let fields = self.match_args(*c);
                    for (i, a) in args.iter().enumerate() {
                        let f = self.find_field(*c, &fields[i]).unwrap();
                        let sub = CE::new(f.ty.clone(), format!("({})->f_{}", narrowed.c, c_ident(&fields[i])));
                        self.bind_pattern(a, &sub)?;
                    }
                    for (k, a) in kwargs {
                        let f = self.find_field(*c, k).unwrap();
                        let sub = CE::new(f.ty.clone(), format!("({})->f_{}", narrowed.c, c_ident(k)));
                        self.bind_pattern(a, &sub)?;
                    }
                } else if args.len() == 1 {
                    self.bind_pattern(&args[0], &narrowed)?;
                }
                Ok(())
            }
            Pattern::Sequence(ps, _) => {
                let star = ps.iter().position(|p| matches!(p, Pattern::Star(_)));
                if let Ty::Tuple(ts) = &subj.ty {
                    if (star.is_none() && ts.len() != ps.len()) || ts.len() + 1 < ps.len() {
                        return Ok(()); // can never match (see pattern())
                    }
                }
                for (i, x) in ps.iter().enumerate() {
                    match x {
                        Pattern::Star(Some(n)) => {
                            let after = ps.len() - i - 1;
                            match &subj.ty {
                                Ty::List(_) => {
                                    let sl = format!("fpy_list_slice({}, true, {}, {}, {}, 1)", subj.c, i, after > 0, -(after as i64));
                                    self.store_name(n, CE::new(subj.ty.clone(), sl), Span::default())?;
                                }
                                Ty::Tuple(ts) => {
                                    // the starred part of a tuple binds a list of its (uniform) element type
                                    let part = &ts[i..ts.len() - after];
                                    let et = part.first().cloned().unwrap_or(Ty::Int);
                                    if part.iter().any(|t| *t != et) {
                                        return Err(self.e(Span::default(), format!("starred pattern '{}' would bind a list of mixed types", n)));
                                    }
                                    let items: Vec<String> = (i..ts.len() - after).map(|k| format!("fpy_list_append(_sl, ({})->items[{}]);", subj.c, k)).collect();
                                    let code = format!("({{ FpyList *_sl = fpy_list_new({}, {}); {} _sl; }})", et.kind(), part.len(), items.join(" "));
                                    self.store_name(n, CE::new(Ty::List(Box::new(et)), code), Span::default())?;
                                }
                                _ => {}
                            }
                        }
                        Pattern::Star(None) => {}
                        _ => {
                            let (idx, ti) = self.seq_pattern_index(subj, ps.len(), star, i);
                            let t = match &subj.ty {
                                Ty::List(e) => (**e).clone(),
                                Ty::Tuple(ts) => ts[ti].clone(),
                                _ => unreachable!(),
                            };
                            let ct = self.ctype(&t);
                            let item = CE::new(t.clone(), t.from_word(&format!("({})->items[{}]", subj.c, idx), &ct));
                            self.bind_pattern(x, &item)?;
                        }
                    }
                }
                Ok(())
            }
            Pattern::Mapping { keys, values, rest } => {
                if let Ty::Dict(kt, vt) = subj.ty.clone() {
                    let mut key_words = Vec::new();
                    for (k, v) in keys.iter().zip(values) {
                        let kc = self.expr(k, Some(&kt))?;
                        let kc = self.check_coerce(&kc, &kt, k.span)?;
                        let vct = self.ctype(&vt);
                        key_words.push(kt.to_word(&kc));
                        let item = CE::new((*vt).clone(), vt.from_word(&format!("fpy_dict_get({}, {})", subj.c, kt.to_word(&kc)), &vct));
                        self.bind_pattern(v, &item)?;
                    }
                    if let Some(r) = rest {
                        // `**rest` holds the entries not matched by the explicit keys
                        let dels: String = key_words.iter().map(|w| format!("fpy_dict_del(_rest, {}, false); ", w)).collect();
                        let code = format!("({{ FpyDict *_rest = fpy_dict_copy({}); {}_rest; }})", subj.c, dels);
                        self.store_name(r, CE::new(subj.ty.clone(), code), Span::default())?;
                    }
                }
                Ok(())
            }
            Pattern::Or(ps) => {
                // bindings in or-patterns must be identical; bind from the first alternative
                if let Some(first) = ps.first() {
                    let mut names = BTreeSet::new();
                    pattern_names(first, &mut names);
                    if !names.is_empty() {
                        return Err(self.e(Span::default(), "capture patterns inside '|' alternatives are not supported"));
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    // ------------------------------------------------------------------ classes

    fn class_init_stmts(&mut self, cid: ClassId) -> R<()> {
        let def = self.classes[cid].def.clone();
        if self.classes[cid].is_enum {
            let members = self.classes[cid].enum_members.clone();
            let cn = self.classes[cid].cname.clone();
            for (name, val) in members {
                if let Some(g) = self.find_class_var(cid, &name) {
                    let gn = self.globals[g].cname.clone();
                    let s = self.string_lit(&name);
                    self.emit(&format!("G.{} = new_{}({}, {});", gn, cn, s, val));
                }
            }
            return Ok(());
        }
        for st in &def.body {
            match &st.kind {
                StmtKind::Assign { targets, value } => {
                    for t in targets {
                        if let ExprKind::Name(n) = &t.kind {
                            if let Some(g) = self.find_class_var(cid, n) {
                                let want = self.globals[g].ty.clone();
                                let ce = self.expr(value, want.as_ref())?;
                                if self.globals[g].ty.is_none() {
                                    let ty = self.infer_var_type(&ce.ty, t.span, n)?;
                                    self.globals[g].ty = Some(ty);
                                }
                                let gt = self.globals[g].ty.clone().unwrap();
                                let c = self.check_coerce(&ce, &gt, value.span)?;
                                let gn = self.globals[g].cname.clone();
                                self.emit(&format!("G.{} = {};", gn, c));
                            }
                        }
                    }
                }
                StmtKind::AnnAssign { target, value: Some(value), ann } if is_classvar_ann(ann) || matches!(&target.kind, ExprKind::Name(n) if self.classes[cid].class_vars.iter().any(|(c, _)| c == n)) => {
                    if let ExprKind::Name(n) = &target.kind {
                        if let Some(g) = self.find_class_var(cid, n) {
                            let want = self.globals[g].ty.clone();
                            let ce = self.expr(value, want.as_ref())?;
                            if self.globals[g].ty.is_none() {
                                self.globals[g].ty = Some(ce.ty.clone());
                            }
                            let gt = self.globals[g].ty.clone().unwrap();
                            let c = self.check_coerce(&ce, &gt, value.span)?;
                            let gn = self.globals[g].cname.clone();
                            self.emit(&format!("G.{} = {};", gn, c));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Emit struct, constructor, vtable, dynamic wrappers and type descriptor for a class.
    pub fn emit_class(&mut self, cid: ClassId) -> R<()> {
        if self.emitted_types.contains(&cid) {
            return Ok(());
        }
        self.emitted_types.insert(cid);
        self.resolve_class(cid)?;
        let cls_name = self.classes[cid].name.clone();
        let cn = self.classes[cid].cname.clone();
        let m = self.classes[cid].module;
        let fields = self.classes[cid].fields.clone();
        // struct
        let mut st = format!("struct {} {{\n    const FpyType *ty;\n", cn);
        for f in &fields {
            st.push_str(&format!("    {} f_{};\n", self.ctype(&f.ty), c_ident(&f.name)));
        }
        st.push_str("};\n");
        self.out.structs.push_str(&st);
        // methods: compile all non-generic methods
        let methods = self.classes[cid].methods.clone();
        for (_, fid) in &methods {
            if self.funcs[*fid].def.type_params.is_empty() {
                self.check_override(cid, *fid)?;
                self.func_instance(*fid, vec![])?;
            }
        }
        // constructor
        let init = self.find_method(cid, "__init__");
        let (ctor_params, ctor_args, init_call) = match init {
            Some(f) if !self.classes[cid].is_enum => {
                let sig = self.func_sig(f)?;
                let mut ps = Vec::new();
                let mut args = vec![format!("(struct {}*)self", self.classes[self.funcs[f].owner.unwrap()].cname)];
                // same order as bind_args: positional, *args, keyword-only, **kwargs
                let mut order: Vec<(String, Ty)> = sig.params.iter().skip(1).map(|p| (p.name.clone(), p.ty.clone())).collect();
                if let Some((n, t)) = &sig.vararg {
                    let pos = sig.params.iter().skip(1).position(|p| p.kwonly).unwrap_or(order.len());
                    order.insert(pos, (n.clone(), t.clone()));
                }
                if let Some((n, t)) = &sig.kwarg {
                    order.push((n.clone(), t.clone()));
                }
                for (n, t) in &order {
                    ps.push(format!("{} p_{}", self.ctype(t), c_ident(n)));
                    args.push(format!("p_{}", c_ident(n)));
                }
                let fname = self.func_instance(f, vec![])?;
                (ps, args.clone(), format!("    {}({});\n", fname, args.join(", ")))
            }
            _ if self.classes[cid].is_enum => (
                vec!["FpyStr *p_name".to_string(), "fpy_int p_value".to_string()],
                vec![],
                "    self->f_name = p_name;\n    self->f_value = p_value;\n".to_string(),
            ),
            _ => (vec![], vec![], String::new()),
        };
        let _ = ctor_args;
        let pc = if ctor_params.is_empty() { "void".to_string() } else { ctor_params.join(", ") };
        self.out.protos.push_str(&format!("static struct {cn} *new_{cn}({pc});\n"));
        let mut ctor = format!("static struct {cn} *new_{cn}({pc}) {{\n    struct {cn} *self = fpy_alloc(&T_{cn}, sizeof(struct {cn}));\n");
        // field defaults declared in the class body (non-dataclass)
        if !self.classes[cid].is_dataclass {
            let mut fx = Fx::new(m, Some(cid));
            fx.subst = self.classes[cid].subst.clone();
            self.fx_stack.push(fx);
            let mut inits = String::new();
            let r = (|| -> R<()> {
                for f in &fields {
                    if let Some(d) = &f.default {
                        let ce = self.expr(d, Some(&f.ty))?;
                        let c = self.check_coerce(&ce, &f.ty, d.span)?;
                        inits.push_str(&format!("    self->f_{} = {};\n", c_ident(&f.name), c));
                    }
                }
                Ok(())
            })();
            let fx = self.fx_stack.pop().unwrap();
            r?;
            if !fx.decls.is_empty() {
                ctor.push_str("    {\n");
                for d in &fx.decls {
                    ctor.push_str(&format!("    {}\n", d));
                }
                ctor.push_str(&fx.body);
                ctor.push_str("    }\n");
            }
            ctor.push_str(&inits);
        }
        ctor.push_str(&init_call);
        ctor.push_str("    return self;\n}\n\n");
        self.out.funcs.push_str(&ctor);

        // pointer offsets
        let ptrs: Vec<String> = fields.iter().filter(|f| f.ty.kind() == FK_OBJ).map(|f| format!("offsetof(struct {}, f_{})", cn, c_ident(&f.name))).collect();
        let mut ty = String::new();
        ty.push_str(&format!("static const int32_t PO_{}[] = {{{}}};\n", cn, if ptrs.is_empty() { "0".to_string() } else { ptrs.join(", ") }));
        let ftab: Vec<String> = fields.iter().map(|f| format!("{{{}, offsetof(struct {}, f_{}), {}}}", c_string(f.name.as_bytes()), cn, c_ident(&f.name), f.ty.kind())).collect();
        ty.push_str(&format!("static const FpyField FT_{}[] = {{{}}};\n", cn, if ftab.is_empty() { "{0}".to_string() } else { ftab.join(", ") }));
        // dynamic method wrappers
        let mut mtab = Vec::new();
        for (name, fid) in &methods {
            if !self.funcs[*fid].def.type_params.is_empty() {
                continue;
            }
            if let Ok(w) = self.dyn_wrapper(*fid) {
                mtab.push(format!("{{{}, {}}}", c_string(name.as_bytes()), w));
            }
        }
        ty.push_str(&format!("static const FpyMethod MT_{}[] = {{{}}};\n", cn, if mtab.is_empty() { "{0}".to_string() } else { mtab.join(", ") }));
        // protocol slots
        let mut slots = String::new();
        let objt = Ty::Class(cid);
        if let Some(f) = self.find_method(cid, "__repr__") {
            let n = self.method_cname(f);
            let oc = self.classes[self.funcs[f].owner.unwrap()].cname.clone();
            ty.push_str(&format!("static FpyStr *RP_{cn}(FpyObj *o) {{ return {n}((struct {oc}*)o); }}\n"));
            slots.push_str(&format!(".repr = RP_{cn}, "));
        } else if self.classes[cid].is_enum {
            ty.push_str(&format!("static FpyStr *RP_{cn}(FpyObj *o) {{ FpySB sb; fpy_sb_init(&sb); fpy_sb_add_cstr(&sb, \"<{}.\"); fpy_sb_add_str(&sb, ((struct {cn}*)o)->f_name); fpy_sb_add_cstr(&sb, \": \"); fpy_sb_add_str(&sb, fpy_int_to_str(((struct {cn}*)o)->f_value)); fpy_sb_add_cstr(&sb, \">\"); return fpy_sb_finish(&sb); }}\n", cls_name));
            ty.push_str(&format!("static FpyStr *ST_{cn}(FpyObj *o) {{ FpySB sb; fpy_sb_init(&sb); fpy_sb_add_cstr(&sb, \"{}.\"); fpy_sb_add_str(&sb, ((struct {cn}*)o)->f_name); return fpy_sb_finish(&sb); }}\n", cls_name));
            slots.push_str(&format!(".repr = RP_{cn}, .str = ST_{cn}, "));
        }
        if let Some(f) = self.find_method(cid, "__str__") {
            let n = self.method_cname(f);
            let oc = self.classes[self.funcs[f].owner.unwrap()].cname.clone();
            ty.push_str(&format!("static FpyStr *SS_{cn}(FpyObj *o) {{ return {n}((struct {oc}*)o); }}\n"));
            slots.push_str(&format!(".str = SS_{cn}, "));
        }
        if let Some(f) = self.find_method(cid, "__eq__") {
            let n = self.method_cname(f);
            let sig = self.func_sig(f)?;
            let oc = self.classes[self.funcs[f].owner.unwrap()].cname.clone();
            let other = &sig.params.get(1).map(|p| p.ty.clone()).unwrap_or(Ty::Any);
            let conv = match other {
                Ty::Any => "b".to_string(),
                Ty::Class(oc2) => {
                    let oc2n = self.classes[*oc2].cname.clone();
                    ty.push_str("");
                    format!("(struct {}*)b", oc2n)
                }
                _ => "b".to_string(),
            };
            let guard = match other {
                Ty::Class(oc2) => format!("if (!fpy_isinstance_t(b, &T_{})) return false; ", self.classes[*oc2].cname),
                _ => String::new(),
            };
            ty.push_str(&format!("static bool EQ_{cn}(FpyObj *a, FpyObj *b) {{ {guard}return {n}((struct {oc}*)a, {conv}); }}\n"));
            slots.push_str(&format!(".eq = EQ_{cn}, "));
            if self.find_method(cid, "__hash__").is_none() {
                ty.push_str(&format!("static int64_t HU_{cn}(FpyObj *a) {{ (void)a; fpy_raise_fmt(FPY_EXC_TYPE, \"unhashable type: '%s'\", \"{}\"); }}\n", cls_name));
                slots.push_str(&format!(".hash = HU_{cn}, "));
            }
        }
        if let Some(f) = self.find_method(cid, "__hash__") {
            let n = self.method_cname(f);
            let oc = self.classes[self.funcs[f].owner.unwrap()].cname.clone();
            ty.push_str(&format!("static int64_t HS_{cn}(FpyObj *o) {{ return {n}((struct {oc}*)o); }}\n"));
            slots.push_str(&format!(".hash = HS_{cn}, "));
        }
        if let Some(f) = self.find_method(cid, "__lt__") {
            let n = self.method_cname(f);
            let sig = self.func_sig(f)?;
            let oc = self.classes[self.funcs[f].owner.unwrap()].cname.clone();
            let (guard, conv) = match sig.params.get(1).map(|p| &p.ty) {
                Some(Ty::Class(oc2)) => (format!("if (!fpy_isinstance_t(b, &T_{})) return -1; ", self.classes[*oc2].cname), format!("(struct {}*)b", self.classes[*oc2].cname)),
                _ => (String::new(), "b".to_string()),
            };
            ty.push_str(&format!("static int LT_{cn}(FpyObj *a, FpyObj *b) {{ {guard}return {n}((struct {oc}*)a, {conv}) ? 1 : 0; }}\n"));
            slots.push_str(&format!(".lt = LT_{cn}, "));
        }
        // vtable
        let vslots = self.vslots(cid);
        let mut vt = Vec::new();
        for name in &vslots {
            match self.find_method(cid, name) {
                Some(f) if self.funcs[f].def.type_params.is_empty() && self.funcs[f].kind != FnKind::Static && self.funcs[f].kind != FnKind::ClassMethod => {
                    vt.push(format!("(void*){}", self.method_cname(f)))
                }
                _ => vt.push("NULL".into()),
            }
        }
        ty.push_str(&format!("static void *VT_{}[] = {{{}}};\n", cn, if vt.is_empty() { "NULL".to_string() } else { vt.join(", ") }));
        // dynamic constructor
        let ctor_dyn = self.dyn_ctor(cid)?;
        let base = match self.classes[cid].base {
            Some(b) => format!("&T_{}", self.classes[b].cname),
            None => "NULL".into(),
        };
        ty.push_str(&format!(
            "const FpyType T_{cn} = {{.name = {}, .base = {}, .tag = FT_INSTANCE, .size = sizeof(struct {cn}), .ptr_offsets = PO_{cn}, .nptrs = {}, {}.fields = FT_{cn}, .nfields = {}, .methods = MT_{cn}, .nmethods = {}, .vtable = VT_{cn}, .ctor = {}}};\n\n",
            c_string(cls_name.as_bytes()),
            base,
            ptrs.len(),
            slots,
            fields.len(),
            mtab.len(),
            ctor_dyn
        ));
        let _ = objt;
        self.out.types.push_str(&ty);
        Ok(())
    }

    fn check_override(&mut self, cid: ClassId, fid: usize) -> R<()> {
        let name = self.funcs[fid].name.clone();
        if name.starts_with("__") && name.ends_with("__") {
            return Ok(());
        }
        if let Some(b) = self.classes[cid].base {
            if let Some(bf) = self.find_method(b, &name) {
                let a = self.func_sig(fid)?;
                let bs = self.func_sig(bf)?;
                let ok = a.params.len() == bs.params.len()
                    && a.params.iter().zip(&bs.params).skip(1).all(|(x, y)| x.ty == y.ty)
                    && (a.ret == bs.ret || (self.ctype(&a.ret) == self.ctype(&bs.ret) && self.assignable(&a.ret, &bs.ret)) || matches!((&a.ret, &bs.ret), (Ty::Class(_), Ty::Class(_)) if self.assignable(&a.ret, &bs.ret)));
                if !ok {
                    let m = self.funcs[fid].module;
                    return Err(self.err(m, self.classes[cid].span, format!("method '{}.{}' overrides '{}' with an incompatible signature", self.classes[cid].name, name, self.funcs[bf].qualname)));
                }
                self.funcs[fid].overrides = Some(bf);
            }
        }
        Ok(())
    }

    /// Virtual method slot names for a class (base slots first, stable across subclasses).
    pub fn vslots(&self, cid: ClassId) -> Vec<String> {
        let mut v = match self.classes[cid].base {
            Some(b) => self.vslots(b),
            None => vec![],
        };
        for (n, _) in &self.classes[cid].methods {
            if !v.contains(n) {
                v.push(n.clone());
            }
        }
        v
    }

    /// Dynamic-call wrapper for a method or function: FpyObj *(void *env, FpyObj **a, int64_t n).
    pub fn dyn_wrapper(&mut self, fid: usize) -> R<String> {
        let target = self.func_instance(fid, vec![])?;
        if let Some(w) = self.dyn_wrappers.get(&target) {
            return Ok(w.clone());
        }
        let sig = self.func_sig(fid)?;
        if sig.kwarg.is_some() {
            return Err(self.err(self.funcs[fid].module, Span::default(), "no dynamic wrapper for **kwargs"));
        }
        let w = format!("DW_{}", target);
        self.dyn_wrappers.insert(target.clone(), w.clone());
        let m = self.funcs[fid].module;
        let required = sig.params.iter().filter(|p| p.default.is_none()).count();
        let mut fx = Fx::new(m, self.funcs[fid].owner);
        fx.subst = self.funcs[fid].subst.clone();
        self.fx_stack.push(fx);
        let mut args = Vec::new();
        let r = (|| -> R<()> {
            for (i, p) in sig.params.iter().enumerate() {
                let un = if i == 0 && self.funcs[fid].kind == FnKind::Method {
                    let oc = self.classes[self.funcs[fid].owner.unwrap()].cname.clone();
                    format!("((struct {}*)a[0])", oc)
                } else {
                    self.unbox(&format!("a[{}]", i), &p.ty)
                };
                match &p.default {
                    Some(d) => {
                        let ce = self.expr(d, Some(&p.ty))?;
                        let dc = self.check_coerce(&ce, &p.ty, d.span)?;
                        args.push(format!("((n > {} && a[{}] != fpy_missing) ? {} : {})", i, i, un, dc));
                    }
                    None => args.push(un),
                }
            }
            Ok(())
        })();
        let fx = self.fx_stack.pop().unwrap();
        r?;
        let np = sig.params.len();
        if let Some((_, vt)) = &sig.vararg {
            // extra positional arguments are packed into the *args list
            let et = match vt {
                Ty::List(e) => (**e).clone(),
                _ => Ty::Any,
            };
            let item = self.unbox("a[_k]", &et);
            let pos = sig.params.iter().position(|p| p.kwonly).unwrap_or(np);
            args.insert(pos, format!("({{ FpyList *_va = fpy_list_new({}, 4); for (int64_t _k = {}; _k < n; _k++) fpy_list_append(_va, {}); _va; }})", et.kind(), np, et.to_word(&item)));
        }
        let call = format!("{}({})", target, args.join(", "));
        let ret = if sig.ret == Ty::NoneT || sig.ret == Ty::Never { format!("{}; return NULL;", call) } else { format!("return {};", self.coerce(&call, &sig.ret, &Ty::Any)) };
        let maxn = if sig.vararg.is_some() { "INT64_MAX".to_string() } else { np.to_string() };
        let mut code = format!(
            "static FpyObj *{w}(void *env, FpyObj **a, int64_t n) {{\n    (void)env;\n    if (n < {} || n > {}) fpy_raise_fmt(FPY_EXC_TYPE, \"%s() takes %d to %d arguments (%lld given)\", {}, {}, {}, (long long)n);\n",
            required,
            maxn,
            c_string(self.funcs[fid].qualname.as_bytes()),
            required,
            sig.params.len()
        );
        for d in &fx.decls {
            code.push_str(&format!("    {}\n", d));
        }
        code.push_str(&fx.body);
        code.push_str(&format!("    {}\n}}\n\n", ret));
        self.out.protos.push_str(&format!("static FpyObj *{w}(void *env, FpyObj **a, int64_t n);\n"));
        self.out.funcs.push_str(&code);
        Ok(w)
    }

    fn dyn_ctor(&mut self, cid: ClassId) -> R<String> {
        let cn = self.classes[cid].cname.clone();
        if self.classes[cid].is_enum {
            return Ok("NULL".into());
        }
        let init = self.find_method(cid, "__init__");
        let (params, defaults_ok) = match init {
            Some(f) => {
                let sig = self.func_sig(f)?;
                (sig.params[1..].to_vec(), sig.vararg.is_none() && sig.kwarg.is_none())
            }
            None => (vec![], true),
        };
        if !defaults_ok {
            return Ok("NULL".into());
        }
        // defaults are compiled where `__init__` is defined (it may be inherited from another module)
        let m = init.map(|f| self.funcs[f].module).unwrap_or(self.classes[cid].module);
        let mut fx = Fx::new(m, Some(cid));
        fx.subst = self.classes[cid].subst.clone();
        self.fx_stack.push(fx);
        let mut args = Vec::new();
        let r = (|| -> R<()> {
            for (i, p) in params.iter().enumerate() {
                let un = self.unbox(&format!("a[{}]", i), &p.ty);
                match &p.default {
                    Some(d) => {
                        let ce = self.expr(d, Some(&p.ty))?;
                        let dc = self.check_coerce(&ce, &p.ty, d.span)?;
                        args.push(format!("(n > {} ? {} : {})", i, un, dc));
                    }
                    None => args.push(un),
                }
            }
            Ok(())
        })();
        let fx = self.fx_stack.pop().unwrap();
        r?;
        let required = params.iter().filter(|p| p.default.is_none()).count();
        let mut code = format!(
            "static FpyObj *DC_{cn}(void *env, FpyObj **a, int64_t n) {{\n    (void)env; (void)a;\n    if (n < {} || n > {}) fpy_raise_code(FPY_EXC_TYPE, \"wrong number of constructor arguments\");\n",
            required,
            params.len()
        );
        for d in &fx.decls {
            code.push_str(&format!("    {}\n", d));
        }
        code.push_str(&fx.body);
        code.push_str(&format!("    return (FpyObj*)new_{}({});\n}}\n\n", cn, args.join(", ")));
        self.out.protos.push_str(&format!("static FpyObj *DC_{cn}(void *env, FpyObj **a, int64_t n);\n"));
        self.out.funcs.push_str(&code);
        Ok(format!("DC_{}", cn))
    }
}

fn is_classvar_ann(ann: &Expr) -> bool {
    match &ann.kind {
        ExprKind::Name(n) => n == "ClassVar",
        ExprKind::Subscript { value, .. } => matches!(&value.kind, ExprKind::Name(n) if n == "ClassVar"),
        _ => false,
    }
}

fn pattern_names(p: &Pattern, out: &mut BTreeSet<String>) {
    match p {
        Pattern::Capture(n) => {
            out.insert(n.clone());
        }
        Pattern::As(p, n) => {
            out.insert(n.clone());
            pattern_names(p, out);
        }
        Pattern::Or(ps) | Pattern::Sequence(ps, _) => ps.iter().for_each(|p| pattern_names(p, out)),
        Pattern::Class { args, kwargs, .. } => {
            args.iter().for_each(|p| pattern_names(p, out));
            kwargs.iter().for_each(|(_, p)| pattern_names(p, out));
        }
        Pattern::Star(Some(n)) => {
            out.insert(n.clone());
        }
        _ => {}
    }
}

#[allow(dead_code)]
fn unused(_: &scope::Scope) {}
