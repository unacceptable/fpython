//! Calls: builtins, user functions (incl. generics), methods, constructors, builtin-type methods.

use super::expr::CompKind;
use super::func::{Local, CE};
use super::*;

pub const BUILTIN_FUNCS: &[&str] = &[
    "print", "len", "range", "str", "int", "float", "bool", "abs", "min", "max", "sum", "sorted", "reversed", "enumerate", "zip", "isinstance",
    "input", "list", "dict", "set", "frozenset", "tuple", "repr", "ord", "chr", "hex", "oct", "bin", "round", "divmod", "pow", "any", "all",
    "map", "filter", "iter", "next", "hash", "id", "cast", "super", "type", "callable", "format", "bytes", "issubclass",
    "getattr", "hasattr", "setattr", "ascii",
];

impl Compiler {
    pub fn call(&mut self, func: &Expr, args: &[Expr], kws: &[Keyword], want: Option<&Ty>, span: Span) -> R<CE> {
        match &func.kind {
            ExprKind::Name(n) => {
                // local/global callable values first
                if !self.is_builtin_name(n) || !BUILTIN_FUNCS.contains(&n.as_str()) {
                    match self.lookup_sym(self.cur_mod(), n) {
                        Some(Sym::Func(f)) if self.is_builtin_name(n) || matches!(self.modules[self.cur_mod()].scope.get(n), Some(Sym::Func(_))) => {
                            if !self.fxr().locals.contains_key(n) && !self.fxr().env.contains_key(n) && !self.fxr().comp.iter().any(|c| c.contains_key(n)) {
                                return self.call_func(f, args, kws, want, span);
                            }
                        }
                        Some(Sym::Class(c)) if !self.fxr().locals.contains_key(n) => return self.construct(c, args.to_vec(), kws.to_vec(), span),
                        Some(Sym::ClassTemplate(t)) if !self.fxr().locals.contains_key(n) => return self.construct_template(t, args, kws, want, span),
                        Some(Sym::Builtin(b)) if b == "cast" || b == "dataclass" || b == "auto" => {
                            if b == "cast" {
                                return self.builtin_call("cast", args, kws, want, span);
                            }
                        }
                        _ => {}
                    }
                } else if BUILTIN_FUNCS.contains(&n.as_str()) {
                    return self.builtin_call(n, args, kws, want, span);
                }
            }
            ExprKind::Attribute { value, attr } => return self.method_call(value, attr, args, kws, want, span),
            ExprKind::Subscript { value, index } => {
                // Generic class instantiation: Stack[int](...)
                if let ExprKind::Name(n) = &value.kind {
                    if let Some(Sym::ClassTemplate(_)) = self.lookup_sym(self.cur_mod(), n) {
                        let m = self.cur_mod();
                        let subst = self.fxr().subst.clone();
                        let t = self.resolve_ann(&Expr::new(ExprKind::Subscript { value: value.clone(), index: index.clone() }, func.span), m, &subst)?;
                        if let Ty::Class(c) = t {
                            return self.construct(c, args.to_vec(), kws.to_vec(), span);
                        }
                    }
                }
                // Explicit generic function instantiation: f[int](...)
                if let ExprKind::Name(n) = &value.kind {
                    if let Some(Sym::Func(f)) = self.lookup_sym(self.cur_mod(), n) {
                        let m = self.cur_mod();
                        let subst = self.fxr().subst.clone();
                        let targs_e: Vec<Expr> = match &index.kind {
                            ExprKind::Tuple { elts, .. } => elts.clone(),
                            _ => vec![(**index).clone()],
                        };
                        let mut targs = Vec::new();
                        for t in &targs_e {
                            targs.push(self.resolve_ann(t, m, &subst)?);
                        }
                        return self.call_func_targs(f, Some(targs), args, kws, want, span);
                    }
                }
            }
            ExprKind::Lambda { params, body } => {
                // immediately-invoked lambda: infer parameter types from the arguments
                let mut ces = Vec::new();
                for a in args {
                    ces.push(self.expr(a, None)?);
                }
                let ft = Ty::Func(ces.iter().map(|c| c.ty.clone()).collect(), Box::new(Ty::Var("?".into())));
                let lam = Expr::new(ExprKind::Lambda { params: params.clone(), body: body.clone() }, func.span);
                let f = self.expr(&lam, Some(&ft))?;
                return self.call_value(f, ces, span);
            }
            _ => {}
        }
        // A nested `def` called by name: bind arguments with its full signature.
        if let ExprKind::Name(n) = &func.kind {
            if let Some(sig) = self.nested_sig(n) {
                let callee = self.expr(func, None)?;
                if let Ty::Func(ptys, _) = callee.ty.clone() {
                    let m = self.cur_mod();
                    let subst = self.fxr().subst.clone();
                    let cargs = self.bind_args(&sig, args, kws, span, m, n, &subst)?;
                    let ces = cargs.into_iter().zip(ptys).map(|(c, t)| CE::new(t, c)).collect();
                    return self.call_value(callee, ces, span);
                }
            }
        }
        // Calling a value of function type (or Any, or a class object).
        let callee = self.expr(func, None)?;
        match callee.ty.clone() {
            Ty::Func(ps, _) => {
                if !kws.is_empty() {
                    return Err(self.e(span, "keyword arguments are not supported when calling a function value"));
                }
                if args.len() != ps.len() {
                    return Err(self.e(span, format!("function value expects {} arguments, got {}", ps.len(), args.len())));
                }
                let mut ces = Vec::new();
                for (a, p) in args.iter().zip(&ps) {
                    let ce = self.expr(a, Some(p))?;
                    ces.push(ce);
                }
                self.call_value(callee, ces, span)
            }
            // a class named directly is constructed statically; a `type[C]` value may hold a
            // subclass, so it goes through the class object's dynamic constructor
            Ty::Type(c) if callee.c == format!("fpy_class_obj(&T_{})", self.classes[c].cname) => self.construct(c, args.to_vec(), kws.to_vec(), span),
            Ty::Type(c) => {
                let (arr, n, kn, kv, nk) = self.dyn_args(args, kws, span)?;
                let v = format!("fpy_dyn_call_kw({}, {}, {}, {}, {}, {})", callee.c, arr, n, kn, kv, nk);
                Ok(CE::new(Ty::Class(c), self.unbox(&v, &Ty::Class(c))))
            }
            Ty::Any => {
                let (arr, n, kn, kv, nk) = self.dyn_args(args, kws, span)?;
                Ok(CE::new(Ty::Any, format!("fpy_dyn_call_kw({}, {}, {}, {}, {}, {})", callee.c, arr, n, kn, kv, nk)))
            }
            Ty::Class(c) if self.find_method(c, "__call__").is_some() => {
                let f = self.find_method(c, "__call__").unwrap();
                self.call_method_ce(callee, f, args.to_vec(), None, span)
            }
            t => Err(self.e(span, format!("'{}' object is not callable", self.tyname(&t)))),
        }
    }

    /// Box arguments for a dynamic (Any) call: (args array, n, kw names array, kw values array, nk).
    pub fn dyn_args(&mut self, args: &[Expr], kws: &[Keyword], span: Span) -> R<(String, usize, String, String, usize)> {
        let mut boxed = Vec::new();
        for a in args {
            if let ExprKind::Starred(_) = a.kind {
                return Err(self.e(a.span, "*args splats in dynamic calls are not supported yet"));
            }
            // lambdas passed to dynamic callees take Any parameters
            let want = match &a.kind {
                ExprKind::Lambda { params, .. } => Some(Ty::Func(vec![Ty::Any; params.all_positional().count()], Box::new(Ty::Var("?".into())))),
                _ => None,
            };
            let ce = self.expr(a, want.as_ref())?;
            boxed.push(self.coerce(&ce.c, &ce.ty, &Ty::Any));
        }
        let mut names = Vec::new();
        let mut vals = Vec::new();
        for k in kws {
            let n = k.name.clone().ok_or_else(|| self.e(span, "**kwargs splats in dynamic calls are not supported yet"))?;
            let want = match &k.value.kind {
                ExprKind::Lambda { params, .. } => Some(Ty::Func(vec![Ty::Any; params.all_positional().count()], Box::new(Ty::Var("?".into())))),
                _ => None,
            };
            let ce = self.expr(&k.value, want.as_ref())?;
            names.push(c_string(n.as_bytes()));
            vals.push(self.coerce(&ce.c, &ce.ty, &Ty::Any));
        }
        let arr = if boxed.is_empty() { "NULL".to_string() } else { format!("(FpyObj*[]){{{}}}", boxed.join(", ")) };
        let kn = if names.is_empty() { "NULL".to_string() } else { format!("(const char*[]){{{}}}", names.join(", ")) };
        let kv = if vals.is_empty() { "NULL".to_string() } else { format!("(FpyObj*[]){{{}}}", vals.join(", ")) };
        Ok((arr, boxed.len(), kn, kv, names.len()))
    }

    /// Call a function-typed value with already-compiled arguments.
    pub fn call_value(&mut self, f: CE, args: Vec<CE>, span: Span) -> R<CE> {
        let (ps, ret) = match &f.ty {
            Ty::Func(ps, r) => (ps.clone(), (**r).clone()),
            _ => unreachable!(),
        };
        let mut cargs = vec!["_f->env".to_string()];
        let mut ptys = vec!["void*".to_string()];
        for (a, p) in args.iter().zip(&ps) {
            cargs.push(self.check_coerce(a, p, span)?);
            ptys.push(self.ctype(p));
        }
        let rc = if ret == Ty::NoneT { "void".to_string() } else { self.ctype(&ret) };
        Ok(CE::new(ret, format!("({{ FpyFunc *_f = {}; (({}(*)({}))_f->fn)({}); }})", f.c, rc, ptys.join(", "), cargs.join(", "))))
    }

    // ------------------------------------------------------------------ user functions

    pub fn call_func(&mut self, f: usize, args: &[Expr], kws: &[Keyword], want: Option<&Ty>, span: Span) -> R<CE> {
        self.call_func_targs(f, None, args, kws, want, span)
    }

    fn call_func_targs(&mut self, f: usize, explicit: Option<Vec<Ty>>, args: &[Expr], kws: &[Keyword], want: Option<&Ty>, span: Span) -> R<CE> {
        let sig = self.func_sig(f)?;
        let tparams: Vec<String> = self.funcs[f].def.type_params.iter().map(|t| t.name.clone()).collect();
        if tparams.is_empty() {
            let cargs = self.bind_args(&sig, args, kws, span, self.funcs[f].module, &self.funcs[f].name.clone(), &HashMap::new())?;
            let name = self.func_instance(f, vec![])?;
            return Ok(CE::new(sig.ret.clone(), format!("{}({})", name, cargs.join(", "))));
        }
        // generic: infer type arguments
        let mut map: HashMap<String, Ty> = HashMap::new();
        if let Some(ex) = explicit {
            if ex.len() != tparams.len() {
                return Err(self.e(span, format!("expected {} type arguments", tparams.len())));
            }
            for (n, t) in tparams.iter().zip(ex) {
                map.insert(n.clone(), t);
            }
        } else {
            let pos: Vec<&Param> = sig.params.iter().filter(|p| !p.kwonly).collect();
            for (i, a) in args.iter().enumerate() {
                let pty = match pos.get(i) {
                    Some(p) => p.ty.clone(),
                    None => match &sig.vararg {
                        Some((_, Ty::List(e))) => (**e).clone(),
                        _ => return Err(self.e(a.span, "too many arguments")),
                    },
                };
                // use what is already inferred (e.g. `total(xs, len)` once T is known from xs)
                let partial = pty.subst(&map);
                let w = if partial.has_var() { None } else { Some(partial.clone()) };
                let saved = self.fxr().dry_run;
                self.fx().dry_run = true;
                let ce = match &a.kind {
                    ExprKind::Lambda { .. } => {
                        let partial = pty.subst(&map);
                        if partial.has_var() {
                            if let Ty::Func(ps, _) = &partial {
                                if ps.iter().any(|p| p.has_var()) {
                                    self.fx().dry_run = saved;
                                    continue;
                                }
                                let w2 = Ty::Func(ps.clone(), Box::new(Ty::Var("?".into())));
                                self.expr(a, Some(&w2))
                            } else {
                                self.expr(a, w.as_ref())
                            }
                        } else {
                            self.expr(a, Some(&partial))
                        }
                    }
                    _ => self.expr(a, w.as_ref()),
                };
                self.fx().dry_run = saved;
                let ce = ce?;
                self.infer_tvars(&pty, &ce.ty, &mut map);
            }
            for k in kws {
                if let Some(name) = &k.name {
                    if let Some(p) = sig.params.iter().find(|p| &p.name == name) {
                        let saved = self.fxr().dry_run;
                        self.fx().dry_run = true;
                        let ce = self.expr(&k.value, None);
                        self.fx().dry_run = saved;
                        self.infer_tvars(&p.ty, &ce?.ty, &mut map);
                    }
                }
            }
            if let Some(w) = want {
                self.infer_tvars(&sig.ret, w, &mut map);
            }
        }
        let mut targs = Vec::new();
        for n in &tparams {
            match map.get(n) {
                Some(t) if !has_unknown(t) => targs.push(t.clone()),
                _ => return Err(self.e(span, format!("cannot infer type parameter '{}' of '{}'; pass it explicitly as {}[...](...)", n, self.funcs[f].name, self.funcs[f].name))),
            }
        }
        // bounds
        for (tp, t) in self.funcs[f].def.type_params.clone().iter().zip(&targs) {
            if let Some(b) = &tp.bound {
                let m = self.funcs[f].module;
                let bt = self.resolve_ann(b, m, &HashMap::new())?;
                if !self.assignable(t, &bt) {
                    return Err(self.e(span, format!("type {} does not satisfy bound {} of '{}'", self.tyname(t), self.tyname(&bt), tp.name)));
                }
            }
        }
        let sub_sig = Sig {
            params: sig.params.iter().map(|p| Param { ty: p.ty.subst(&map), ..p.clone() }).collect(),
            vararg: sig.vararg.as_ref().map(|(n, t)| (n.clone(), t.subst(&map))),
            kwarg: sig.kwarg.as_ref().map(|(n, t)| (n.clone(), t.subst(&map))),
            ret: sig.ret.subst(&map),
        };
        let cargs = self.bind_args(&sub_sig, args, kws, span, self.funcs[f].module, &self.funcs[f].name.clone(), &map)?;
        let name = self.func_instance(f, targs)?;
        Ok(CE::new(sub_sig.ret, format!("{}({})", name, cargs.join(", "))))
    }

    pub fn infer_tvars(&self, p: &Ty, a: &Ty, map: &mut HashMap<String, Ty>) {
        match (p, a) {
            (Ty::Var(n), t) => {
                if has_unknown(t) {
                    return;
                }
                match map.get(n) {
                    None => {
                        map.insert(n.clone(), t.clone());
                    }
                    Some(prev) => {
                        if let Some(u) = self.unify(prev, t) {
                            map.insert(n.clone(), u);
                        }
                    }
                }
            }
            (Ty::List(x), Ty::List(y)) | (Ty::Set(x), Ty::Set(y)) | (Ty::Iter(x), Ty::Iter(y)) | (Ty::Optional(x), Ty::Optional(y)) | (Ty::VarTuple(x), Ty::VarTuple(y)) => {
                self.infer_tvars(x, y, map)
            }
            (Ty::VarTuple(x), Ty::Tuple(ts)) => {
                for t in ts {
                    self.infer_tvars(x, t, map);
                }
            }
            (Ty::Iter(x), t) => {
                if let Some(e) = elem_of(t) {
                    self.infer_tvars(x, &e, map)
                }
            }
            (Ty::Optional(x), t) => self.infer_tvars(x, t, map),
            (Ty::Dict(k1, v1), Ty::Dict(k2, v2)) => {
                self.infer_tvars(k1, k2, map);
                self.infer_tvars(v1, v2, map);
            }
            (Ty::Tuple(xs), Ty::Tuple(ys)) if xs.len() == ys.len() => {
                for (x, y) in xs.iter().zip(ys) {
                    self.infer_tvars(x, y, map);
                }
            }
            (Ty::Func(ps, r), Ty::Func(qs, s)) if ps.len() == qs.len() => {
                for (x, y) in ps.iter().zip(qs) {
                    self.infer_tvars(x, y, map);
                }
                self.infer_tvars(r, s, map);
            }
            (Ty::Union(ms), t) => {
                // T | None style handled above; for unions pick the single var member
                let vars: Vec<&Ty> = ms.iter().filter(|m| m.has_var()).collect();
                if vars.len() == 1 {
                    self.infer_tvars(vars[0], t, map);
                }
            }
            _ => {}
        }
    }

    /// The signature of the nested `def` that `n` refers to, looking out through enclosing
    /// functions until a scope binds `n` some other way.
    fn nested_sig(&self, n: &str) -> Option<Sig> {
        for fx in self.fx_stack.iter().rev() {
            if let Some(s) = fx.nested_sigs.get(n) {
                return Some(s.clone());
            }
            if fx.locals.contains_key(n) || fx.is_module {
                return None;
            }
        }
        None
    }

    /// Bind call arguments to parameters, filling defaults; returns C argument expressions.
    pub fn bind_args(&mut self, sig: &Sig, args: &[Expr], kws: &[Keyword], span: Span, callee_mod: usize, fname: &str, tmap: &HashMap<String, Ty>) -> R<Vec<String>> {
        let n = sig.params.len();
        let mut slots: Vec<Option<String>> = vec![None; n];
        let mut extra: Vec<(CE, Span)> = Vec::new();
        let mut splat: Option<CE> = None;
        let positional: Vec<usize> = (0..n).filter(|i| !sig.params[*i].kwonly).collect();
        let mut pi = 0;
        for a in args {
            if let ExprKind::Starred(inner) = &a.kind {
                if let Some((_, vt)) = &sig.vararg {
                    if pi >= positional.len() {
                        let ce = self.expr(inner, Some(vt))?;
                        splat = Some(ce);
                        continue;
                    }
                }
                // splat a tuple into positional parameters
                let ce = self.expr(inner, None)?;
                if let Ty::Tuple(ts) = &ce.ty {
                    let tv = self.tmp_var(&ce.ty, "sp");
                    self.emit(&format!("{} = {};", tv, ce.c));
                    for (k, t) in ts.iter().enumerate() {
                        let ct = self.ctype(t);
                        let item = CE::new(t.clone(), t.from_word(&format!("{}->items[{}]", tv, k), &ct));
                        if pi >= positional.len() {
                            return Err(self.e(a.span, format!("too many arguments for {}()", fname)));
                        }
                        let p = &sig.params[positional[pi]];
                        slots[positional[pi]] = Some(self.check_coerce(&item, &p.ty, a.span)?);
                        pi += 1;
                    }
                    continue;
                }
                return Err(self.e(a.span, "*args splat requires a tuple or a function with *args"));
            }
            if pi < positional.len() {
                let p = sig.params[positional[pi]].clone();
                let ce = self.expr(a, Some(&p.ty))?;
                slots[positional[pi]] = Some(self.check_coerce(&ce, &p.ty, a.span).map_err(|d| d.note(format!("in argument '{}' of {}()", p.name, fname)))?);
                pi += 1;
            } else if let Some((_, Ty::List(et))) = &sig.vararg {
                let ce = self.expr(a, Some(et))?;
                extra.push((ce, a.span));
            } else {
                return Err(self.e(a.span, format!("{}() takes {} positional arguments but {} were given", fname, positional.len(), args.len())));
            }
        }
        let mut kwextra: Vec<(String, CE, Span)> = Vec::new();
        for k in kws {
            let name = match &k.name {
                Some(n) => n,
                None => return Err(self.e(span, "**kwargs splat in calls is not supported")),
            };
            match sig.params.iter().position(|p| &p.name == name) {
                Some(i) => {
                    if slots[i].is_some() {
                        return Err(self.e(k.value.span, format!("{}() got multiple values for argument '{}'", fname, name)));
                    }
                    let p = sig.params[i].clone();
                    let ce = self.expr(&k.value, Some(&p.ty))?;
                    slots[i] = Some(self.check_coerce(&ce, &p.ty, k.value.span)?);
                }
                None => match &sig.kwarg {
                    Some((_, Ty::Dict(_, vt))) => {
                        let ce = self.expr(&k.value, Some(vt))?;
                        kwextra.push((name.clone(), ce, k.value.span));
                    }
                    _ => return Err(self.e(k.value.span, format!("{}() got an unexpected keyword argument '{}'", fname, name))),
                },
            }
        }
        // defaults: compiled in the callee's module
        let mut out = Vec::new();
        for (i, p) in sig.params.iter().enumerate() {
            match slots[i].take() {
                Some(c) => out.push(c),
                None => match &p.default {
                    Some(d) => {
                        let saved_mod = self.fxr().module;
                        self.fx().module = callee_mod;
                        let saved_subst = std::mem::replace(&mut self.fx().subst, tmap.clone());
                        let r = self.expr(d, Some(&p.ty)).and_then(|ce| self.check_coerce(&ce, &p.ty, d.span));
                        self.fx().subst = saved_subst;
                        self.fx().module = saved_mod;
                        out.push(r?);
                    }
                    None => return Err(self.e(span, format!("{}() missing required argument '{}'", fname, p.name))),
                },
            }
        }
        if let Some((_, vt)) = &sig.vararg {
            let et = match vt {
                Ty::List(e) => (**e).clone(),
                _ => unreachable!(),
            };
            let pos = sig.params.iter().position(|p| p.kwonly).unwrap_or(sig.params.len());
            let c = if let Some(s) = splat {
                self.check_coerce(&s, vt, span)?
            } else {
                let mut code = format!("({{ FpyList *_va = fpy_list_new({}, {}); ", et.kind(), extra.len());
                for (ce, sp) in &extra {
                    let c = self.check_coerce(ce, &et, *sp)?;
                    code.push_str(&format!("fpy_list_append(_va, {}); ", et.to_word(&c)));
                }
                code.push_str("_va; })");
                code
            };
            out.insert(pos, c);
        }
        if let Some((_, kt)) = &sig.kwarg {
            let vt = match kt {
                Ty::Dict(_, v) => (**v).clone(),
                _ => unreachable!(),
            };
            let mut code = format!("({{ FpyDict *_kw = fpy_dict_new(FK_OBJ, {}); ", vt.kind());
            for (n, ce, sp) in &kwextra {
                let c = self.check_coerce(ce, &vt, *sp)?;
                let key = self.string_lit(n);
                code.push_str(&format!("fpy_dict_set(_kw, fpy_wp({}), {}); ", key, vt.to_word(&c)));
            }
            code.push_str("_kw; })");
            out.push(code);
        }
        Ok(out)
    }

    // ------------------------------------------------------------------ constructors

    pub fn construct(&mut self, c: ClassId, args: Vec<Expr>, kws: Vec<Keyword>, span: Span) -> R<CE> {
        self.resolve_class(c)?;
        if self.classes[c].is_enum {
            return Err(self.e(span, "enum members cannot be constructed; use the member attributes"));
        }
        if self.classes[c].is_protocol {
            return Err(self.e(span, "protocols cannot be instantiated"));
        }
        let cn = self.classes[c].cname.clone();
        let name = self.classes[c].name.clone();
        match self.find_method(c, "__init__") {
            Some(f) => {
                let sig = self.func_sig(f)?;
                let sig2 = Sig { params: sig.params[1..].to_vec(), ..sig.clone() };
                let m = self.funcs[f].module;
                let subst = self.classes[c].subst.clone();
                let cargs = self.bind_args(&sig2, &args, &kws, span, m, &name, &subst)?;
                Ok(CE::new(Ty::Class(c), format!("new_{}({})", cn, cargs.join(", "))))
            }
            None => {
                if !args.is_empty() || !kws.is_empty() {
                    return Err(self.e(span, format!("{}() takes no arguments", name)));
                }
                Ok(CE::new(Ty::Class(c), format!("new_{}()", cn)))
            }
        }
    }

    fn construct_template(&mut self, t: usize, args: &[Expr], kws: &[Keyword], want: Option<&Ty>, span: Span) -> R<CE> {
        // use the expected type when it is an instance of this template
        if let Some(Ty::Class(c)) = want {
            if self.templates[t].instances.values().any(|x| x == c) {
                return self.construct(*c, args.to_vec(), kws.to_vec(), span);
            }
        }
        // infer from __init__ parameter annotations
        let def = self.templates[t].def.clone();
        let m = self.templates[t].module;
        let vars: HashMap<String, Ty> = def.type_params.iter().map(|p| (p.name.clone(), Ty::Var(p.name.clone()))).collect();
        let init = def.body.iter().find_map(|s| match &s.kind {
            StmtKind::FuncDef(f) if f.name == "__init__" => Some(f.clone()),
            _ => None,
        });
        let mut map = HashMap::new();
        if let Some(init) = init {
            let ps: Vec<&Arg> = init.params.all_positional().skip(1).collect();
            for (a, p) in args.iter().zip(ps) {
                if let Some(ann) = &p.ann {
                    let pt = self.resolve_ann(ann, m, &vars)?;
                    let saved = self.fxr().dry_run;
                    self.fx().dry_run = true;
                    let ce = self.expr(a, None);
                    self.fx().dry_run = saved;
                    self.infer_tvars(&pt, &ce?.ty, &mut map);
                }
            }
        }
        let mut targs = Vec::new();
        for p in &def.type_params {
            match map.get(&p.name) {
                Some(t) => targs.push(t.clone()),
                None => return Err(self.e(span, format!("cannot infer type parameter '{}' of {}; write {}[...](...)", p.name, def.name, def.name))),
            }
        }
        let c = self.instantiate_template(t, targs, self.cur_mod(), span)?;
        self.emit_class_later(c);
        self.construct(c, args.to_vec(), kws.to_vec(), span)
    }

    fn emit_class_later(&mut self, _c: ClassId) {
        // classes are emitted by the main loop in compile_program (it scans new classes).
    }

    // ------------------------------------------------------------------ methods

    /// Call method `f` on an already-compiled receiver with argument expressions.
    pub fn call_method_ce(&mut self, recv: CE, f: usize, args: Vec<Expr>, extra: Option<CE>, span: Span) -> R<CE> {
        let sig = self.func_sig(f)?;
        let owner = self.funcs[f].owner.unwrap();
        let name = self.funcs[f].name.clone();
        let mut sig2 = Sig { params: sig.params[1..].to_vec(), ..sig.clone() };
        let mut extra_c = Vec::new();
        if let Some(x) = extra {
            // trailing already-compiled argument (e.g. value for __setitem__)
            let p = sig2.params.pop().ok_or_else(|| self.e(span, "too many arguments"))?;
            extra_c.push(self.check_coerce(&x, &p.ty, span)?);
        }
        let m = self.funcs[f].module;
        let subst = self.classes[owner].subst.clone();
        let mut cargs = self.bind_args(&sig2, &args, &[], span, m, &name, &subst)?;
        cargs.extend(extra_c);
        self.dispatch(recv, f, cargs, &sig.ret, span)
    }

    pub fn call_method_with_ces(&mut self, recv: CE, f: usize, args: Vec<CE>, span: Span) -> R<CE> {
        let sig = self.func_sig(f)?;
        if args.len() + 1 != sig.params.len() {
            return Err(self.e(span, format!("{} expects {} arguments", self.funcs[f].qualname, sig.params.len() - 1)));
        }
        let mut cargs = Vec::new();
        for (a, p) in args.iter().zip(sig.params.iter().skip(1)) {
            cargs.push(self.check_coerce(a, &p.ty, span)?);
        }
        let ret = sig.ret.clone();
        self.dispatch(recv, f, cargs, &ret, span)
    }

    fn dispatch(&mut self, recv: CE, f: usize, cargs: Vec<String>, ret: &Ty, span: Span) -> R<CE> {
        let owner = self.funcs[f].owner.unwrap();
        let oc = self.classes[owner].cname.clone();
        let name = self.funcs[f].name.clone();
        let static_cls = match &recv.ty {
            Ty::Class(c) => *c,
            _ => owner,
        };
        if !self.funcs[f].def.type_params.is_empty() {
            return Err(self.e(span, "generic methods are not supported yet; make the class generic instead"));
        }
        if self.classes[static_cls].is_protocol {
            // structural typing: the receiver's own method is found by name at runtime
            let sig = self.func_sig(f)?;
            let mut boxed = Vec::new();
            for (c, p) in cargs.iter().zip(sig.params.iter().skip(1)) {
                boxed.push(self.coerce(c, &p.ty, &Ty::Any));
            }
            let arr = if boxed.is_empty() { "NULL".to_string() } else { format!("(FpyObj*[]){{{}}}", boxed.join(", ")) };
            let call = format!("fpy_dyn_call_method((FpyObj*)({}), {}, {}, {})", recv.c, c_string(name.as_bytes()), arr, boxed.len());
            if *ret == Ty::NoneT {
                return Ok(CE::new(Ty::NoneT, format!("((void)({}))", call)));
            }
            let c = self.unbox(&call, ret);
            return Ok(CE::new(ret.clone(), c));
        }
        let virt = self.is_overridden(static_cls, &name) && self.funcs[f].kind == FnKind::Method;
        if !virt {
            let target = self.func_instance(f, vec![])?;
            let mut all = vec![format!("(struct {}*)({})", oc, recv.c)];
            all.extend(cargs);
            return Ok(CE::new(ret.clone(), format!("{}({})", target, all.join(", "))));
        }
        let slots = self.vslots(static_cls);
        let slot = slots.iter().position(|s| *s == name).unwrap();
        let sig = self.func_sig(f)?;
        let rc = if *ret == Ty::NoneT { "void".to_string() } else { self.ctype(ret) };
        let mut ptys = vec![format!("struct {}*", oc)];
        for p in sig.params.iter().skip(1) {
            ptys.push(self.ctype(&p.ty));
        }
        let rt = self.fresh("rv");
        let mut all = vec![format!("(struct {}*){}", oc, rt)];
        all.extend(cargs);
        let rct = self.ctype(&recv.ty);
        Ok(CE::new(ret.clone(), format!("({{ {} {} = {}; (({}(*)({}))((const FpyObj*){})->ty->vtable[{}])({}); }})", rct, rt, recv.c, rc, ptys.join(", "), rt, slot, all.join(", "))))
    }

    fn method_call(&mut self, value: &Expr, attr: &str, args: &[Expr], kws: &[Keyword], want: Option<&Ty>, span: Span) -> R<CE> {
        // module.function(...)
        if let Some(mid) = self.module_path(value) {
            match self.modules[mid].scope.get(attr).cloned() {
                Some(Sym::Func(f)) => return self.call_func(f, args, kws, want, span),
                Some(Sym::Class(c)) => return self.construct(c, args.to_vec(), kws.to_vec(), span),
                Some(Sym::ClassTemplate(t)) => return self.construct_template(t, args, kws, want, span),
                Some(Sym::Global(_)) => {
                    let v = self.module_attr(mid, attr, span)?;
                    let fe = Expr::new(ExprKind::Name("\u{1}callee".into()), span);
                    self.fx().comp.push(HashMap::from([("\u{1}callee".to_string(), Local { ty: v.ty.clone(), cname: format!("({})", v.c), cell: false })]));
                    let r = self.call(&fe, args, kws, want, span);
                    self.fx().comp.pop();
                    return r;
                }
                _ => return Err(self.e(span, format!("module '{}' has no function '{}'", self.modules[mid].name, attr))),
            }
        }
        if let ExprKind::Name(n) = &value.kind {
            if let Some(Sym::Builtin(b)) = self.lookup_sym(self.cur_mod(), n) {
                if b.starts_with("module:") && self.is_builtin_name(n) {
                    return Err(self.e(span, format!("'{}.{}' is not supported", n, attr)));
                }
            }
        }
        // super().method(...)
        if let ExprKind::Call { func, args: sargs, .. } = &value.kind {
            if matches!(&func.kind, ExprKind::Name(n) if n == "super") && sargs.is_empty() && self.fxr().py_class.is_some() {
                // method of a Python-derived class: Python's super(cls, self)
                let g = self.fxr().py_class.unwrap();
                let gn = self.globals[g].cname.clone();
                let me = self.load_name("self", span)?;
                let (arr, n, kn, kv, nk) = self.dyn_args(args, kws, span)?;
                return Ok(CE::new(Ty::Any, format!("fpy_dyn_call_method_kw(fpy_py_super(G.{}, {}), {}, {}, {}, {}, {}, {})", gn, me.c, c_string(attr.as_bytes()), arr, n, kn, kv, nk)));
            }
            if matches!(&func.kind, ExprKind::Name(n) if n == "super") && sargs.is_empty() {
                let cls = self.fxr().class.ok_or_else(|| self.e(span, "super() outside a method"))?;
                let base = self.classes[cls].base.ok_or_else(|| self.e(span, "super(): class has no base class"))?;
                let f = self.find_method(base, attr).ok_or_else(|| self.e(span, format!("base class has no method '{}'", attr)))?;
                let me = self.load_name("self", span)?;
                let sig = self.func_sig(f)?;
                let sig2 = Sig { params: sig.params[1..].to_vec(), ..sig.clone() };
                let m = self.funcs[f].module;
                let owner = self.funcs[f].owner.unwrap();
                let subst = self.classes[owner].subst.clone();
                let cargs = self.bind_args(&sig2, args, kws, span, m, attr, &subst)?;
                let target = self.func_instance(f, vec![])?;
                let mut all = vec![format!("(struct {}*)({})", self.classes[owner].cname, me.c)];
                all.extend(cargs);
                return Ok(CE::new(sig.ret.clone(), format!("{}({})", target, all.join(", "))));
            }
            // type(x).__name__ etc. handled in attribute()
        }
        // Class.method(...) — static/class methods and unbound calls
        if let ExprKind::Name(n) = &value.kind {
            let is_cls = matches!(self.lookup_sym(self.cur_mod(), n), Some(Sym::Class(_))) && !self.fxr().locals.contains_key(n);
            if is_cls {
                if let Some(Sym::Class(c)) = self.lookup_sym(self.cur_mod(), n) {
                    return self.class_method_call(c, attr, args, kws, span);
                }
            }
        }
        let recv = self.expr(value, None)?;
        match recv.ty.clone() {
            Ty::Class(c) => {
                if let Some(f) = self.find_method(c, attr) {
                    match self.funcs[f].kind {
                        FnKind::Static => return self.call_func(f, args, kws, want, span),
                        FnKind::ClassMethod => return self.class_method_call(c, attr, args, kws, span),
                        FnKind::Property => {
                            let v = self.call_method_ce(recv, f, vec![], None, span)?;
                            return self.call_ce_value(v, args, kws, span);
                        }
                        _ => {}
                    }
                    let sig = self.func_sig(f)?;
                    let sig2 = Sig { params: sig.params[1..].to_vec(), ..sig.clone() };
                    let m = self.funcs[f].module;
                    let owner = self.funcs[f].owner.unwrap();
                    let subst = self.classes[owner].subst.clone();
                    let rv = self.tmp_var(&recv.ty, "recv");
                    let cargs = self.bind_args(&sig2, args, kws, span, m, attr, &subst)?;
                    let call = self.dispatch(CE::new(recv.ty.clone(), rv.clone()), f, cargs, &sig.ret, span)?;
                    return Ok(CE::new(call.ty, format!("({{ {} = {}; {}; }})", rv, recv.c, call.c)));
                }
                if let Some(fl) = self.find_field(c, attr) {
                    let v = CE::new(fl.ty.clone(), format!("({})->f_{}", recv.c, c_ident(attr)));
                    return self.call_ce_value(v, args, kws, span);
                }
                Err(self.e(span, format!("'{}' object has no method '{}'", self.classes[c].name, attr)))
            }
            Ty::Type(c) => self.class_method_call(c, attr, args, kws, span),
            Ty::Any => {
                let (arr, n, kn, kv, nk) = self.dyn_args(args, kws, span)?;
                Ok(CE::new(Ty::Any, format!("fpy_dyn_call_method_kw({}, {}, {}, {}, {}, {}, {})", recv.c, c_string(attr.as_bytes()), arr, n, kn, kv, nk)))
            }
            Ty::Optional(_) => Err(self.e(span, format!("value may be None; check `is not None` before calling '.{}()'", attr))),
            _ => self.builtin_method(recv, attr, args, kws, want, span),
        }
    }

    fn call_ce_value(&mut self, v: CE, args: &[Expr], kws: &[Keyword], span: Span) -> R<CE> {
        let fe = Expr::new(ExprKind::Name("\u{1}callee".into()), span);
        let tv = self.tmp_var(&v.ty, "cv");
        self.emit(&format!("{} = {};", tv, v.c));
        self.fx().comp.push(HashMap::from([("\u{1}callee".to_string(), Local { ty: v.ty.clone(), cname: tv, cell: false })]));
        let r = self.call(&fe, args, kws, None, span);
        self.fx().comp.pop();
        r
    }

    fn class_method_call(&mut self, c: ClassId, attr: &str, args: &[Expr], kws: &[Keyword], span: Span) -> R<CE> {
        let f = match self.find_method(c, attr) {
            Some(f) => f,
            None => {
                if self.classes[c].is_enum && attr == "__call__" {
                    return Err(self.e(span, "enum lookup by value is not supported"));
                }
                return Err(self.e(span, format!("class '{}' has no method '{}'", self.classes[c].name, attr)));
            }
        };
        match self.funcs[f].kind {
            FnKind::Static => self.call_func(f, args, kws, None, span),
            FnKind::ClassMethod => {
                let sig = self.func_sig(f)?;
                let sig2 = Sig { params: sig.params[1..].to_vec(), ..sig.clone() };
                let m = self.funcs[f].module;
                let cargs = self.bind_args(&sig2, args, kws, span, m, attr, &HashMap::new())?;
                let target = self.func_instance(f, vec![])?;
                let mut all = vec![format!("fpy_class_obj(&T_{})", self.classes[c].cname)];
                all.extend(cargs);
                let ret = sig.ret.subst(&HashMap::new());
                Ok(CE::new(ret, format!("{}({})", target, all.join(", "))))
            }
            _ => {
                // unbound method call: Class.method(obj, ...)
                if args.is_empty() {
                    return Err(self.e(span, "unbound method call needs an instance argument"));
                }
                let recv = self.expr(&args[0], Some(&Ty::Class(c)))?;
                let rc = self.check_coerce(&recv, &Ty::Class(c), args[0].span)?;
                let sig = self.func_sig(f)?;
                let sig2 = Sig { params: sig.params[1..].to_vec(), ..sig.clone() };
                let m = self.funcs[f].module;
                let cargs = self.bind_args(&sig2, &args[1..], kws, span, m, attr, &HashMap::new())?;
                let target = self.func_instance(f, vec![])?;
                let owner = self.funcs[f].owner.unwrap();
                let mut all = vec![format!("(struct {}*)({})", self.classes[owner].cname, rc)];
                all.extend(cargs);
                Ok(CE::new(sig.ret.clone(), format!("{}({})", target, all.join(", "))))
            }
        }
    }

    // ------------------------------------------------------------------ builtin functions

    fn arg(&mut self, args: &[Expr], i: usize, want: Option<&Ty>, fname: &str, span: Span) -> R<CE> {
        match args.get(i) {
            Some(a) => self.expr(a, want),
            None => Err(self.e(span, format!("{}() missing argument {}", fname, i + 1))),
        }
    }

    fn kw<'a>(&self, kws: &'a [Keyword], name: &str) -> Option<&'a Expr> {
        kws.iter().find(|k| k.name.as_deref() == Some(name)).map(|k| &k.value)
    }

    fn expect_int(&mut self, e: &Expr) -> R<String> {
        let ce = self.expr(e, Some(&Ty::Int))?;
        self.check_coerce(&ce, &Ty::Int, e.span)
    }

    fn builtin_call(&mut self, name: &str, args: &[Expr], kws: &[Keyword], want: Option<&Ty>, span: Span) -> R<CE> {
        let n = args.len();
        match name {
            "print" => {
                let sep = match self.kw(kws, "sep") {
                    Some(e) => {
                        let ce = self.expr(e, Some(&Ty::Str))?;
                        Some(self.check_coerce(&ce, &Ty::Str, e.span)?)
                    }
                    None => None,
                };
                let end = match self.kw(kws, "end") {
                    Some(e) => {
                        let ce = self.expr(e, Some(&Ty::Str))?;
                        Some(self.check_coerce(&ce, &Ty::Str, e.span)?)
                    }
                    None => None,
                };
                let to_err = match self.kw(kws, "file") {
                    Some(Expr { kind: ExprKind::Attribute { value, attr }, .. }) if matches!(&value.kind, ExprKind::Name(m) if m == "sys") => attr == "stderr",
                    Some(e) => return Err(self.e(e.span, "print(file=...) supports only sys.stdout and sys.stderr")),
                    None => false,
                };
                let flush = self.kw(kws, "flush").is_some();
                let mut parts = Vec::new();
                for a in args {
                    if let ExprKind::Starred(inner) = &a.kind {
                        let ce = self.expr(inner, None)?;
                        let et = self.elem_type(&ce.ty).ok_or_else(|| self.e(a.span, "cannot unpack non-iterable"))?;
                        let ic = self.coerce(&ce.c, &ce.ty, &Ty::iter(et.clone()));
                        let ect = self.ctype(&et);
                        let item = CE::new(et.clone(), et.from_word("_pw", &ect));
                        let s = self.to_str(&item);
                        let sepc = sep.clone().unwrap_or_else(|| self.string_lit(" "));
                        parts.push(format!("{{ FpyIter *_pi = {}; fpy_word _pw; bool _pf = true; while (_pi->next(_pi, &_pw)) {{ if (!_pf) fpy_sb_add_str(&_ps, {}); _pf = false; fpy_sb_add_str(&_ps, {}); }} }}", ic, sepc, s));
                        continue;
                    }
                    // evaluate every argument before stringifying any (Python semantics)
                    let ce = self.expr(a, None)?;
                    let tv = self.tmp_var(&ce.ty, "pa");
                    self.emit(&format!("{} = {};", tv, ce.c));
                    let held = CE::new(ce.ty.clone(), tv);
                    parts.push(format!("fpy_sb_add_str(&_ps, {});", self.to_str(&held)));
                }
                let sepc = sep.unwrap_or_else(|| self.string_lit(" "));
                let endc = end.unwrap_or_else(|| self.string_lit("\n"));
                let mut code = String::from("({ FpySB _ps; fpy_sb_init(&_ps); ");
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        code.push_str(&format!("fpy_sb_add_str(&_ps, {}); ", sepc));
                    }
                    code.push_str(p);
                    code.push(' ');
                }
                code.push_str(&format!("fpy_sb_add_str(&_ps, {}); ", endc));
                if to_err {
                    code.push_str("fpy_write_err(fpy_sb_finish(&_ps)); ");
                } else {
                    code.push_str("fpy_write(fpy_sb_finish(&_ps)); ");
                }
                if flush {
                    code.push_str("fpy_flush(); ");
                }
                code.push_str("})");
                Ok(CE::new(Ty::NoneT, code))
            }
            "len" => {
                let v = self.arg(args, 0, None, name, span)?;
                let c = match &v.ty {
                    Ty::Str => format!("({})->nchars", v.c),
                    Ty::List(_) | Ty::Dict(..) | Ty::Set(_) | Ty::Bytes | Ty::VarTuple(_) => format!("({})->len", v.c),
                    Ty::Tuple(ts) => format!("((void)({}), INT64_C({}))", v.c, ts.len()),
                    Ty::Class(c) => {
                        let f = self.find_method(*c, "__len__").ok_or_else(|| self.e(span, format!("object of type '{}' has no len()", self.classes[*c].name)))?;
                        return self.call_method_ce(v.clone(), f, vec![], None, span);
                    }
                    Ty::Any => format!("fpy_dyn_len({})", v.c),
                    t => return Err(self.e(span, format!("object of type '{}' has no len()", self.tyname(t)))),
                };
                Ok(CE::new(Ty::Int, c))
            }
            "range" => {
                if n == 0 || n > 3 {
                    return Err(self.e(span, "range expects 1 to 3 arguments"));
                }
                let mut cs = Vec::new();
                for a in args {
                    cs.push(self.expect_int(a)?);
                }
                let c = match n {
                    1 => format!("fpy_range_iter(0, {}, 1)", cs[0]),
                    2 => format!("fpy_range_iter({}, {}, 1)", cs[0], cs[1]),
                    _ => format!("fpy_range_iter({}, {}, {})", cs[0], cs[1], cs[2]),
                };
                Ok(CE::new(Ty::iter(Ty::Int), c))
            }
            "str" => {
                if n == 0 {
                    return Ok(CE::new(Ty::Str, self.string_lit("")));
                }
                let v = self.arg(args, 0, None, name, span)?;
                // str(b, encoding) decodes; str(b) is the repr, as in CPython
                if v.ty == Ty::Bytes && n >= 2 {
                    return Ok(CE::new(Ty::Str, format!("fpy_bytes_decode({})", v.c)));
                }
                if v.ty == Ty::Bytes {
                    return Ok(CE::new(Ty::Str, format!("fpy_bytes_repr({})", v.c)));
                }
                Ok(CE::new(Ty::Str, self.to_str(&v)))
            }
            "repr" => {
                let v = self.arg(args, 0, None, name, span)?;
                Ok(CE::new(Ty::Str, self.to_repr(&v)))
            }
            "ascii" => {
                let v = self.arg(args, 0, None, name, span)?;
                Ok(CE::new(Ty::Str, format!("fpy_ascii_escape({})", self.to_repr(&v))))
            }
            "int" => {
                if n == 0 {
                    return Ok(CE::new(Ty::Int, "INT64_C(0)"));
                }
                let v = self.arg(args, 0, None, name, span)?;
                let base = match args.get(1).or(self.kw(kws, "base")) {
                    Some(b) => self.expect_int(b)?,
                    None => "10".into(),
                };
                let c = match &v.ty {
                    Ty::Int => v.c,
                    Ty::Bool => format!("((fpy_int)({}))", v.c),
                    Ty::Float => format!("fpy_float_to_int({})", v.c),
                    Ty::Str => format!("fpy_str_to_int({}, {})", v.c, base),
                    Ty::Bytes => format!("fpy_str_to_int(fpy_bytes_decode({}), {})", v.c, base),
                    Ty::Any | Ty::Union(_) => format!("fpy_dyn_int({})", v.c),
                    Ty::Class(c) => {
                        let f = self.find_method(*c, "__int__").or_else(|| self.find_method(*c, "__index__")).ok_or_else(|| self.e(span, "int() argument must be a string or a number"))?;
                        return self.call_method_ce(v.clone(), f, vec![], None, span);
                    }
                    t => return Err(self.e(span, format!("int() argument must be a string or a number, not {}", self.tyname(t)))),
                };
                Ok(CE::new(Ty::Int, c))
            }
            "float" => {
                if n == 0 {
                    return Ok(CE::new(Ty::Float, "0.0"));
                }
                let v = self.arg(args, 0, None, name, span)?;
                let c = match &v.ty {
                    Ty::Float => v.c,
                    Ty::Int | Ty::Bool => format!("((double)({}))", v.c),
                    Ty::Str => format!("fpy_str_to_float({})", v.c),
                    Ty::Any | Ty::Union(_) => format!("fpy_dyn_float({})", v.c),
                    Ty::Class(c) => {
                        let f = self.find_method(*c, "__float__").ok_or_else(|| self.e(span, "float() argument must be a string or a number"))?;
                        return self.call_method_ce(v.clone(), f, vec![], None, span);
                    }
                    t => return Err(self.e(span, format!("float() argument must be a string or a number, not {}", self.tyname(t)))),
                };
                Ok(CE::new(Ty::Float, c))
            }
            "bool" => {
                if n == 0 {
                    return Ok(CE::new(Ty::Bool, "false"));
                }
                let v = self.arg(args, 0, None, name, span)?;
                Ok(CE::new(Ty::Bool, self.truthy(&v)?))
            }
            "bytes" => {
                if n == 0 {
                    return Ok(CE::new(Ty::Bytes, "fpy_bytes_new(NULL, 0)"));
                }
                let v = self.arg(args, 0, None, name, span)?;
                match &v.ty {
                    Ty::Str => Ok(CE::new(Ty::Bytes, format!("fpy_str_encode({})", v.c))),
                    Ty::List(e) if **e == Ty::Int => Ok(CE::new(Ty::Bytes, format!("({{ FpyList *_l = {}; FpyBytes *_b = fpy_bytes_new(NULL, _l->len); for (int64_t _i = 0; _i < _l->len; _i++) _b->data[_i] = (uint8_t)_l->items[_i]; _b; }})", v.c))),
                    Ty::Int => Ok(CE::new(Ty::Bytes, format!("fpy_bytes_new(NULL, {})", v.c))),
                    Ty::Bytes => Ok(CE::new(Ty::Bytes, format!("({{ FpyBytes *_s = {}; fpy_bytes_new(_s->data, _s->len); }})", v.c))),
                    t => Err(self.e(span, format!("cannot convert {} to bytes", self.tyname(t)))),
                }
            }
            "abs" => {
                let v = self.arg(args, 0, None, name, span)?;
                match &v.ty {
                    Ty::Int | Ty::Bool => Ok(CE::new(Ty::Int, format!("fpy_iabs({})", v.c))),
                    Ty::Float => Ok(CE::new(Ty::Float, format!("fabs({})", v.c))),
                    Ty::Class(c) => {
                        let f = self.find_method(*c, "__abs__").ok_or_else(|| self.e(span, "bad operand type for abs()"))?;
                        self.call_method_ce(v.clone(), f, vec![], None, span)
                    }
                    t => Err(self.e(span, format!("bad operand type for abs(): {}", self.tyname(t)))),
                }
            }
            "min" | "max" => self.minmax(name == "min", args, kws, want, span),
            "sum" => {
                let start = match args.get(1).or(self.kw(kws, "start")) {
                    Some(s) => Some(self.expr(s, None)?),
                    None => None,
                };
                let a0 = args.first().ok_or_else(|| self.e(span, "sum() needs an iterable"))?.clone();
                let et = self.iter_elem_type(&a0)?;
                let acc_ty = match (&et, &start) {
                    (_, Some(s)) if s.ty == Ty::Float => Ty::Float,
                    // an empty literal's element type is unknown: the start value decides
                    (t, Some(s)) if t.has_var() => s.ty.clone(),
                    (t, None) if t.has_var() => Ty::Int,
                    (Ty::Bool, _) => Ty::Int,
                    (t, _) => t.clone(),
                };
                if !acc_ty.is_numeric() && acc_ty != Ty::Any {
                    return Err(self.e(span, format!("sum() of {} is not supported; use ''.join() for strings", self.tyname(&et))));
                }
                let acc = self.fresh("sum");
                let init = match start {
                    Some(s) => self.check_coerce(&s, &acc_ty, span)?,
                    None if acc_ty == Ty::Any => "fpy_box_int(0)".to_string(),
                    None => acc_ty.zero().to_string(),
                };
                if et.has_var() {
                    // only an empty literal has an unknown element type: nothing to add
                    return Ok(CE::new(acc_ty, init));
                }
                let acc2 = acc.clone();
                let at = acc_ty.clone();
                let body = self.reduce_iter(&a0, &mut |c: &mut Compiler, item: CE| {
                    let r = c.binop_ce(CE::new(at.clone(), acc2.clone()), BinOp::Add, item, span)?;
                    c.emit(&format!("{} = {};", acc2, r.c));
                    Ok(())
                })?;
                let ct = self.ctype(&acc_ty);
                Ok(CE::new(acc_ty, format!("({{ {} {} = {}; {} {}; }})", ct, acc, init, body, acc)))
            }
            "any" | "all" => {
                let a0 = args.first().ok_or_else(|| self.e(span, format!("{}() needs an iterable", name)))?.clone();
                let r = self.fresh("q");
                let is_any = name == "any";
                let r2 = r.clone();
                let body = self.reduce_iter(&a0, &mut |c: &mut Compiler, item: CE| {
                    let t = c.truthy(&item)?;
                    if is_any {
                        c.emit(&format!("if ({}) {{ {} = true; break; }}", t, r2));
                    } else {
                        c.emit(&format!("if (!{}) {{ {} = false; break; }}", t, r2));
                    }
                    Ok(())
                })?;
                Ok(CE::new(Ty::Bool, format!("({{ bool {} = {}; {} {}; }})", r, if is_any { "false" } else { "true" }, body, r)))
            }
            "sorted" => {
                let a0 = args.first().ok_or_else(|| self.e(span, "sorted() needs an iterable"))?.clone();
                let lst = self.to_list(&a0, None)?;
                let et = match &lst.ty {
                    Ty::List(e) => (**e).clone(),
                    _ => unreachable!(),
                };
                let rev = match self.kw(kws, "reverse") {
                    Some(r) => {
                        let ce = self.expr(r, Some(&Ty::Bool))?;
                        self.truthy(&ce)?
                    }
                    None => "false".into(),
                };
                let tmp = self.fresh("sl");
                match self.kw(kws, "key").cloned() {
                    Some(k) => {
                        let keys = self.key_list(&k, &tmp, &et)?;
                        Ok(CE::new(lst.ty.clone(), format!("({{ FpyList *{tmp} = {}; if ({tmp}->len > 1) {{ FpyList *_keys = {}; fpy_list_sort_by({tmp}, _keys, {}); }} {tmp}; }})", self.copy_if_needed(&a0, &lst), keys, rev)))
                    }
                    None => {
                        self.check_orderable(&et, span)?;
                        Ok(CE::new(lst.ty.clone(), format!("({{ FpyList *{tmp} = {}; fpy_list_sort({tmp}, {}); {tmp}; }})", self.copy_if_needed(&a0, &lst), rev)))
                    }
                }
            }
            "reversed" => {
                let v = self.arg(args, 0, None, name, span)?;
                match &v.ty {
                    Ty::List(e) => Ok(CE::new(Ty::iter((**e).clone()), format!("fpy_reversed_list({})", v.c))),
                    Ty::Str => Ok(CE::new(Ty::Str, format!("fpy_str_slice({}, false, 0, false, 0, -1)", v.c))),
                    Ty::Iter(e) => Ok(CE::new(Ty::iter((**e).clone()), format!("fpy_reversed_list(fpy_list_from_iter({}, {}))", e.kind(), v.c))),
                    Ty::Class(c) => {
                        let f = self.find_method(*c, "__reversed__").ok_or_else(|| self.e(span, "object is not reversible"))?;
                        self.call_method_ce(v.clone(), f, vec![], None, span)
                    }
                    t => Err(self.e(span, format!("{} is not reversible", self.tyname(t)))),
                }
            }
            "enumerate" => {
                let v = self.arg(args, 0, None, name, span)?;
                let et = self.elem_type(&v.ty).ok_or_else(|| self.e(span, "enumerate() needs an iterable"))?;
                let ic = self.iter_of(&v, &et);
                let start = match args.get(1).or(self.kw(kws, "start")) {
                    Some(s) => self.expect_int(s)?,
                    None => "0".into(),
                };
                Ok(CE::new(Ty::iter(Ty::Tuple(vec![Ty::Int, et])), format!("fpy_enumerate({}, {})", ic, start)))
            }
            "zip" => {
                if n != 2 {
                    return Err(self.e(span, "zip() with other than 2 arguments is only supported directly in for loops"));
                }
                let a = self.arg(args, 0, None, name, span)?;
                let b = self.arg(args, 1, None, name, span)?;
                let ea = self.elem_type(&a.ty).ok_or_else(|| self.e(span, "zip() needs iterables"))?;
                let eb = self.elem_type(&b.ty).ok_or_else(|| self.e(span, "zip() needs iterables"))?;
                let ia = self.iter_of(&a, &ea);
                let ib = self.iter_of(&b, &eb);
                Ok(CE::new(Ty::iter(Ty::Tuple(vec![ea, eb])), format!("fpy_zip({}, {})", ia, ib)))
            }
            "isinstance" => {
                let v = self.arg(args, 0, None, name, span)?;
                let te = args.get(1).ok_or_else(|| self.e(span, "isinstance() needs 2 arguments"))?;
                let (c, _) = self.isinstance_check(&v, te)?;
                Ok(CE::new(Ty::Bool, c))
            }
            "issubclass" => {
                let m = self.cur_mod();
                let subst = self.fxr().subst.clone();
                let a = self.resolve_ann(&args[0], m, &subst)?;
                let b = self.resolve_ann(&args[1], m, &subst)?;
                Ok(CE::new(Ty::Bool, if self.assignable(&a, &b) { "true" } else { "false" }))
            }
            "callable" => {
                let v = self.arg(args, 0, None, name, span)?;
                let r = matches!(v.ty, Ty::Func(..) | Ty::Type(_)) || matches!(&v.ty, Ty::Class(c) if self.find_method(*c, "__call__").is_some());
                Ok(CE::new(Ty::Bool, if r { "true" } else { "false" }))
            }
            "input" => {
                let p = match args.first() {
                    Some(a) => {
                        let ce = self.expr(a, Some(&Ty::Str))?;
                        self.check_coerce(&ce, &Ty::Str, a.span)?
                    }
                    None => "NULL".into(),
                };
                Ok(CE::new(Ty::Str, format!("fpy_input({})", p)))
            }
            "list" => {
                if n == 0 {
                    return Ok(match want {
                        Some(Ty::List(e)) => CE::new(Ty::list((**e).clone()), format!("fpy_list_new({}, 4)", e.kind())),
                        _ => CE::new(Ty::list(Ty::Var("?".into())), "fpy_list_new(FK_OBJ, 4)"),
                    });
                }
                let a0 = args[0].clone();
                let want_e = match want {
                    Some(Ty::List(e)) => Some((**e).clone()),
                    _ => None,
                };
                let lst = self.to_list(&a0, want_e)?;
                Ok(CE::new(lst.ty.clone(), self.copy_if_needed(&a0, &lst)))
            }
            "set" | "frozenset" => {
                if n == 0 {
                    return Ok(match want {
                        Some(Ty::Set(e)) => CE::new(Ty::set((**e).clone()), format!("fpy_set_new({})", e.kind())),
                        _ => CE::new(Ty::set(Ty::Var("?".into())), "fpy_set_new(FK_OBJ)"),
                    });
                }
                if let ExprKind::GenExp { elt, gens } = &args[0].kind {
                    return self.comprehension(CompKind::Set, elt, None, gens, want, span);
                }
                let v = self.expr(&args[0], None)?;
                let et = self.elem_type(&v.ty).ok_or_else(|| self.e(span, "set() needs an iterable"))?;
                let ic = self.iter_of(&v, &et);
                Ok(CE::new(Ty::set(et.clone()), format!("fpy_set_from_iter({}, {})", et.kind(), ic)))
            }
            "dict" => {
                if n == 0 && kws.is_empty() {
                    return Ok(match want {
                        Some(Ty::Dict(k, v)) => CE::new(Ty::dict((**k).clone(), (**v).clone()), format!("fpy_dict_new({}, {})", k.kind(), v.kind())),
                        _ => CE::new(Ty::dict(Ty::Var("?".into()), Ty::Var("?".into())), "fpy_dict_new(FK_OBJ, FK_OBJ)"),
                    });
                }
                if n == 0 {
                    // dict(a=1, b=2)
                    let items: Vec<(Option<Expr>, Expr)> = kws.iter().map(|k| (Some(Expr::new(ExprKind::Str { value: k.name.clone().unwrap_or_default(), raw: vec![] }, span)), k.value.clone())).collect();
                    return self.expr(&Expr::new(ExprKind::Dict(items), span), want);
                }
                let v = self.expr(&args[0], want)?;
                match &v.ty {
                    Ty::Dict(..) => Ok(CE::new(v.ty.clone(), format!("fpy_dict_copy({})", v.c))),
                    t => {
                        let et = self.elem_type(t).ok_or_else(|| self.e(span, "dict() needs a mapping or iterable of pairs"))?;
                        let (kt, vt) = match &et {
                            Ty::Tuple(ts) if ts.len() == 2 => (ts[0].clone(), ts[1].clone()),
                            _ => return Err(self.e(span, "dict() needs an iterable of 2-tuples")),
                        };
                        let ic = self.iter_of(&v, &et);
                        Ok(CE::new(
                            Ty::dict(kt.clone(), vt.clone()),
                            format!("({{ FpyDict *_d = fpy_dict_new({}, {}); FpyIter *_i = {}; fpy_word _w; while (_i->next(_i, &_w)) {{ FpyTuple *_t = fpy_pw(_w); fpy_dict_set(_d, _t->items[0], _t->items[1]); }} _d; }})", kt.kind(), vt.kind(), ic),
                        ))
                    }
                }
            }
            "tuple" => {
                if n == 0 {
                    return Ok(CE::new(Ty::Tuple(vec![]), "fpy_tuple_new(0, NULL)"));
                }
                if let Some(Ty::Tuple(ts)) = want {
                    let lst = self.to_list(&args[0], None)?;
                    let et = match &lst.ty {
                        Ty::List(e) => (**e).clone(),
                        _ => unreachable!(),
                    };
                    if ts.iter().all(|t| *t == et) {
                        let karr = self.kinds_array(&vec![et.kind(); ts.len()]);
                        return Ok(CE::new(
                            Ty::Tuple(ts.clone()),
                            format!("({{ FpyList *_l = {}; if (_l->len != {}) fpy_raise_code(FPY_EXC_VALUE, \"tuple length mismatch\"); FpyTuple *_t = fpy_tuple_new({}, {}); memcpy(_t->items, _l->items, {} * 8); _t; }})", lst.c, ts.len(), ts.len(), karr, ts.len()),
                        ));
                    }
                }
                // tuple(iterable) -> tuple[T, ...]
                let lst = self.to_list(&args[0], None)?;
                let et = match &lst.ty {
                    Ty::List(e) => (**e).clone(),
                    _ => unreachable!(),
                };
                Ok(CE::new(Ty::VarTuple(Box::new(et)), format!("fpy_tuple_from_list({})", lst.c)))
            }
            "ord" => {
                let v = self.arg(args, 0, Some(&Ty::Str), name, span)?;
                let c = self.check_coerce(&v, &Ty::Str, span)?;
                Ok(CE::new(Ty::Int, format!("fpy_str_ord({})", c)))
            }
            "chr" => {
                let c = self.expect_int(&args[0])?;
                Ok(CE::new(Ty::Str, format!("fpy_chr({})", c)))
            }
            "hex" | "oct" | "bin" => {
                let c = self.expect_int(&args[0])?;
                let spec = match name {
                    "hex" => "#x",
                    "oct" => "#o",
                    _ => "#b",
                };
                let s = self.string_lit(spec);
                Ok(CE::new(Ty::Str, format!("fpy_format_int({}, {})", c, s)))
            }
            "format" => {
                let v = self.arg(args, 0, None, name, span)?;
                let spec = match args.get(1) {
                    Some(s) => {
                        let ce = self.expr(s, Some(&Ty::Str))?;
                        self.check_coerce(&ce, &Ty::Str, s.span)?
                    }
                    None => self.string_lit(""),
                };
                let c = match &v.ty {
                    Ty::Int | Ty::Bool => format!("fpy_format_int({}, {})", v.c, spec),
                    Ty::Float => format!("fpy_format_float({}, {})", v.c, spec),
                    _ => {
                        let s = self.to_str(&v);
                        format!("fpy_format_str({}, {})", s, spec)
                    }
                };
                Ok(CE::new(Ty::Str, c))
            }
            "round" => {
                let v = self.arg(args, 0, None, name, span)?;
                match (args.get(1).or(self.kw(kws, "ndigits")), &v.ty) {
                    (None, Ty::Float) => Ok(CE::new(Ty::Int, format!("fpy_round_int({})", v.c))),
                    (None, Ty::Int) => Ok(v),
                    (Some(nd), Ty::Float) => {
                        let ndc = self.expect_int(nd)?;
                        Ok(CE::new(Ty::Float, format!("fpy_round({}, {})", v.c, ndc)))
                    }
                    (Some(_), Ty::Int) => Ok(v),
                    (_, t) => Err(self.e(span, format!("round() of {} is not supported", self.tyname(t)))),
                }
            }
            "divmod" => {
                let a = self.arg(args, 0, None, name, span)?;
                let b = self.arg(args, 1, None, name, span)?;
                if a.ty == Ty::Float || b.ty == Ty::Float {
                    let ac = self.coerce(&a.c, &a.ty, &Ty::Float);
                    let bc = self.coerce(&b.c, &b.ty, &Ty::Float);
                    let karr = self.kinds_array(&[FK_FLOAT, FK_FLOAT]);
                    return Ok(CE::new(Ty::Tuple(vec![Ty::Float, Ty::Float]), format!("({{ double _a = {}, _b = {}; FpyTuple *_t = fpy_tuple_new(2, {}); _t->items[0] = fpy_wf(fpy_ffloordiv(_a, _b)); _t->items[1] = fpy_wf(fpy_fmod(_a, _b)); _t; }})", ac, bc, karr)));
                }
                let karr = self.kinds_array(&[FK_INT, FK_INT]);
                Ok(CE::new(Ty::Tuple(vec![Ty::Int, Ty::Int]), format!("({{ fpy_int _a = {}, _b = {}; FpyTuple *_t = fpy_tuple_new(2, {}); _t->items[0] = (fpy_word)fpy_floordiv(_a, _b); _t->items[1] = (fpy_word)fpy_mod(_a, _b); _t; }})", a.c, b.c, karr)))
            }
            "pow" => {
                let a = self.arg(args, 0, None, name, span)?;
                let b = self.arg(args, 1, None, name, span)?;
                if let Some(m) = args.get(2) {
                    let mc = self.expect_int(m)?;
                    return Ok(CE::new(Ty::Int, format!("({{ fpy_int _b = {}, _e = {}, _m = {}, _r = 1; _b = fpy_mod(_b, _m); while (_e > 0) {{ if (_e & 1) _r = (fpy_int)(((__int128)_r * _b) % _m); _b = (fpy_int)(((__int128)_b * _b) % _m); _e >>= 1; }} _r; }})", a.c, b.c, mc)));
                }
                self.binop_ce(a, BinOp::Pow, b, span)
            }
            "map" | "filter" => {
                if n != 2 {
                    return Err(self.e(span, format!("{}() takes exactly 2 arguments", name)));
                }
                // map(f, xs) == (f(x) for x in xs); filter(f, xs) == (x for x in xs if f(x))
                let x = Expr::new(ExprKind::Name("\u{2}x".into()), span);
                let call = Expr::new(ExprKind::Call { func: Box::new(args[0].clone()), args: vec![x.clone()], keywords: vec![] }, span);
                let gen = if name == "map" {
                    Comprehension { target: x.clone(), iter: args[1].clone(), ifs: vec![], is_async: false }
                } else {
                    let test = if matches!(args[0].kind, ExprKind::NoneLit) { x.clone() } else { call.clone() };
                    Comprehension { target: x.clone(), iter: args[1].clone(), ifs: vec![test], is_async: false }
                };
                let elt = if name == "map" { call } else { x };
                self.genexp(&elt, &[gen], span)
            }
            "iter" => {
                let v = self.arg(args, 0, None, name, span)?;
                let et = self.elem_type(&v.ty).ok_or_else(|| self.e(span, "object is not iterable"))?;
                let ic = self.iter_of(&v, &et);
                Ok(CE::new(Ty::iter(et), ic))
            }
            "next" => {
                let v = self.arg(args, 0, None, name, span)?;
                let et = match &v.ty {
                    Ty::Iter(e) => (**e).clone(),
                    Ty::Any => {
                        return match args.get(1) {
                            Some(d) => {
                                let dv = self.expr(d, None)?;
                                let dc = self.coerce(&dv.c, &dv.ty, &Ty::Any);
                                Ok(CE::new(Ty::Any, format!("fpy_dyn_next_default({}, {})", v.c, dc)))
                            }
                            None => Ok(CE::new(Ty::Any, format!("fpy_dyn_next({})", v.c))),
                        };
                    }
                    Ty::Class(c) => {
                        let f = self.find_method(*c, "__next__").ok_or_else(|| self.e(span, "object is not an iterator"))?;
                        return self.call_method_ce(v.clone(), f, vec![], None, span);
                    }
                    t => return Err(self.e(span, format!("{} is not an iterator", self.tyname(t)))),
                };
                let ct = self.ctype(&et);
                let item = et.from_word("_w", &ct);
                match args.get(1) {
                    Some(d) => {
                        let dc = self.expr(d, Some(&et))?;
                        let dc = self.check_coerce(&dc, &et, d.span)?;
                        Ok(CE::new(et.clone(), format!("({{ FpyIter *_i = {}; fpy_word _w; _i->next(_i, &_w) ? {} : {}; }})", v.c, item, dc)))
                    }
                    None => Ok(CE::new(et.clone(), format!("({{ FpyIter *_i = {}; fpy_word _w; if (!_i->next(_i, &_w)) fpy_raise_stop_iteration(); {}; }})", v.c, item))),
                }
            }
            "hash" => {
                let v = self.arg(args, 0, None, name, span)?;
                let w = v.ty.to_word(&v.c);
                Ok(CE::new(Ty::Int, format!("fpy_hash_word({}, {})", w, v.ty.kind())))
            }
            "id" => {
                let v = self.arg(args, 0, None, name, span)?;
                if v.ty.is_scalar() {
                    return Ok(CE::new(Ty::Int, format!("((fpy_int)({}))", v.c)));
                }
                Ok(CE::new(Ty::Int, format!("((fpy_int)(uintptr_t)({}))", v.c)))
            }
            "cast" => {
                if n != 2 {
                    return Err(self.e(span, "cast() takes a type and a value"));
                }
                let m = self.cur_mod();
                let subst = self.fxr().subst.clone();
                let t = self.resolve_ann(&args[0], m, &subst)?;
                let v = self.expr(&args[1], Some(&t))?;
                if v.ty == t {
                    return Ok(v);
                }
                if v.ty.is_boxed() || v.ty == Ty::Any {
                    let c = self.unbox(&v.c, &t);
                    return Ok(CE::new(t, c));
                }
                match (&v.ty, &t) {
                    (Ty::Class(_), Ty::Class(c)) | (Ty::Optional(_), Ty::Class(c)) => {
                        let cn = self.classes[*c].cname.clone();
                        let nm = self.classes[*c].name.clone();
                        Ok(CE::new(t.clone(), format!("((struct {}*)fpy_check_cast((FpyObj*)({}), &T_{}, {}))", cn, v.c, cn, c_string(nm.as_bytes()))))
                    }
                    _ if self.assignable(&v.ty, &t) => {
                        let c = self.coerce(&v.c, &v.ty, &t);
                        Ok(CE::new(t, c))
                    }
                    _ => Err(self.e(span, format!("cannot cast {} to {}", self.tyname(&v.ty), self.tyname(&t)))),
                }
            }
            "type" => {
                let v = self.arg(args, 0, None, name, span)?;
                match &v.ty {
                    Ty::Class(c) => Ok(CE::new(Ty::Type(*c), format!("fpy_class_obj(((FpyObj*)({}))->ty)", v.c))),
                    _ => Err(self.e(span, "type() is only supported for class instances (use type(x).__name__ for names)")),
                }
            }
            "getattr" | "hasattr" | "setattr" => {
                let obj = self.arg(args, 0, None, name, span)?;
                let nm = args.get(1).ok_or_else(|| self.e(span, format!("{}() needs an attribute name", name)))?;
                // typed object + literal name: an ordinary (statically checked) attribute access
                if let (ExprKind::Str { value, .. }, false) = (&nm.kind, obj.ty == Ty::Any) {
                    let attr = Expr::new(ExprKind::Attribute { value: Box::new(args[0].clone()), attr: value.clone() }, span);
                    return match name {
                        "getattr" => self.expr(&attr, None),
                        "hasattr" => {
                            let ok = self.attr_of(obj.clone(), value, span).is_ok();
                            Ok(CE::new(Ty::Bool, if ok { "true" } else { "false" }))
                        }
                        _ => {
                            let v = self.arg(args, 2, None, name, span)?;
                            self.store_target(&attr, v)?;
                            Ok(CE::new(Ty::NoneT, "((void)0)"))
                        }
                    };
                }
                let o = self.coerce(&obj.c, &obj.ty, &Ty::Any);
                let n = self.expr(nm, Some(&Ty::Str))?;
                let nc = self.check_coerce(&n, &Ty::Str, nm.span)?;
                match name {
                    "getattr" => match args.get(2) {
                        Some(d) => {
                            let dv = self.expr(d, None)?;
                            let dc = self.coerce(&dv.c, &dv.ty, &Ty::Any);
                            Ok(CE::new(Ty::Any, format!("fpy_dyn_getattr_default({}, ({})->data, {})", o, nc, dc)))
                        }
                        None => Ok(CE::new(Ty::Any, format!("fpy_dyn_getattr({}, ({})->data)", o, nc))),
                    },
                    "hasattr" => Ok(CE::new(Ty::Bool, format!("fpy_dyn_hasattr({}, ({})->data)", o, nc))),
                    _ => {
                        let v = self.arg(args, 2, None, name, span)?;
                        let vc = self.coerce(&v.c, &v.ty, &Ty::Any);
                        Ok(CE::new(Ty::NoneT, format!("fpy_dyn_setattr({}, ({})->data, {})", o, nc, vc)))
                    }
                }
            }
            "super" => Err(self.e(span, "super() must be used as super().method(...)")),
            _ => Err(self.e(span, format!("builtin '{}' is not supported", name))),
        }
    }

    fn check_orderable(&mut self, t: &Ty, span: Span) -> R<()> {
        match t {
            Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Bytes | Ty::Tuple(_) | Ty::List(_) | Ty::Any => Ok(()),
            Ty::Class(c) if self.find_method(*c, "__lt__").is_some() => Ok(()),
            _ => Err(self.e(span, format!("{} values are not orderable (define __lt__ or pass key=)", self.tyname(t)))),
        }
    }

    fn copy_if_needed(&self, src: &Expr, lst: &CE) -> String {
        // to_list returns fresh lists except when the source already is a list expression we must not mutate
        if let ExprKind::ListComp { .. } | ExprKind::GenExp { .. } | ExprKind::List(_) = src.kind {
            return lst.c.clone();
        }
        if lst.c.starts_with("fpy_list_from_iter") || lst.c.starts_with("({ FpyList") {
            return lst.c.clone();
        }
        format!("fpy_list_copy({})", lst.c)
    }

    /// Materialize any iterable expression as a list (genexps are inlined).
    pub fn to_list(&mut self, e: &Expr, want_e: Option<Ty>) -> R<CE> {
        if let ExprKind::GenExp { elt, gens } = &e.kind {
            let w = want_e.map(Ty::list);
            return self.comprehension(CompKind::List, elt, None, gens, w.as_ref(), e.span);
        }
        let v = self.expr(e, want_e.clone().map(Ty::list).as_ref())?;
        match &v.ty {
            Ty::List(_) => Ok(v),
            Ty::Any => Ok(CE::new(Ty::list(Ty::Any), format!("fpy_list_from_iter(FK_OBJ, fpy_dyn_iter({}))", v.c))),
            t => {
                let et = self.elem_type(t).ok_or_else(|| self.e(e.span, format!("{} is not iterable", self.tyname(t))))?;
                let ic = self.iter_of(&v, &et);
                Ok(CE::new(Ty::list(et.clone()), format!("fpy_list_from_iter({}, {})", et.kind(), ic)))
            }
        }
    }

    pub fn iter_of(&mut self, v: &CE, et: &Ty) -> String {
        match &v.ty {
            Ty::Iter(_) => v.c.clone(),
            Ty::Any => format!("fpy_dyn_iter({})", v.c),
            _ => self.coerce(&v.c, &v.ty, &Ty::iter(et.clone())),
        }
    }

    fn iter_elem_type(&mut self, e: &Expr) -> R<Ty> {
        if let ExprKind::GenExp { elt, gens } = &e.kind {
            // type of elt in comprehension scope: compile in dry-run
            let saved = self.fxr().dry_run;
            self.fx().dry_run = true;
            let r = self.comprehension(CompKind::List, elt, None, gens, None, e.span);
            self.fx().dry_run = saved;
            return match r?.ty {
                Ty::List(t) => Ok(*t),
                _ => unreachable!(),
            };
        }
        let saved = self.fxr().dry_run;
        self.fx().dry_run = true;
        let r = self.expr(e, None);
        self.fx().dry_run = saved;
        let t = r?.ty;
        self.elem_type(&t).ok_or_else(|| self.e(e.span, format!("{} is not iterable", self.tyname(&t))))
    }

    /// Emit a loop over an iterable expression (genexps inlined) as a C statement block string.
    /// `step` is called with each element; it may `break`.
    fn reduce_iter(&mut self, e: &Expr, step: &mut dyn FnMut(&mut Compiler, CE) -> R<()>) -> R<String> {
        let saved_body = std::mem::take(&mut self.fx().body);
        let saved_indent = self.fxr().indent;
        self.fx().indent = 1;
        self.fx().comp.push(HashMap::new());
        let r = (|| -> R<()> {
            if let ExprKind::GenExp { elt, gens } = &e.kind {
                // inline generator: nested loops, then step(elt)
                let elt = elt.clone();
                let gens = gens.clone();
                return self.inline_gens(&gens, 0, &mut |c: &mut Compiler| {
                    let item = c.expr(&elt, None)?;
                    step(c, item)
                });
            }
            let it_ty = {
                let saved = self.fxr().dry_run;
                self.fx().dry_run = true;
                let r = self.expr(e, None);
                self.fx().dry_run = saved;
                r?.ty
            };
            let et = self.elem_type(&it_ty).ok_or_else(|| self.e(e.span, format!("{} is not iterable", self.tyname(&it_ty))))?;
            let var = format!("\u{2}item{}", self.tmp);
            self.tmp += 1;
            let target = Expr::new(ExprKind::Name(var.clone()), e.span);
            let cname = format!("cv_item_{}", self.tmp);
            self.fx().comp.last_mut().unwrap().insert(var.clone(), Local { ty: et.clone(), cname: cname.clone(), cell: false });
            let et2 = et.clone();
            self.for_loop(&target, e, None, &mut |c: &mut Compiler| step(c, CE::new(et2.clone(), cname.clone())))
        })();
        let scope_vars = self.fx().comp.pop().unwrap();
        let body = std::mem::replace(&mut self.fx().body, saved_body);
        self.fx().indent = saved_indent;
        r?;
        let mut decls = String::new();
        for (_, l) in scope_vars.iter() {
            if l.cname.starts_with("cv_") {
                decls.push_str(&format!("{} {} = {}; ", self.ctype(&l.ty), l.cname, l.ty.zero()));
            }
        }
        Ok(format!("{{ {}\n{} }}", decls, body))
    }

    fn inline_gens(&mut self, gens: &[Comprehension], i: usize, inner: &mut dyn FnMut(&mut Compiler) -> R<()>) -> R<()> {
        // Reuse the comprehension machinery (comp scope already pushed by caller).
        if i == gens.len() {
            return inner(self);
        }
        let g = gens[i].clone();
        let it_ty = {
            let saved = self.fxr().dry_run;
            self.fx().dry_run = true;
            let r = self.expr(&g.iter, None);
            self.fx().dry_run = saved;
            r?.ty
        };
        let et = self.loop_elem_type_pub(&g.iter, &it_ty).ok_or_else(|| self.e(g.iter.span, format!("{} is not iterable", self.tyname(&it_ty))))?;
        self.declare_comp_targets_pub(&g.target, &et)?;
        let gens_v = gens.to_vec();
        self.for_loop(&g.target, &g.iter, None, &mut |c: &mut Compiler| {
            let mut opened = 0;
            for cond in &g.ifs {
                let (cc, pos) = c.cond(cond)?;
                c.emit(&format!("if ({}) {{", cc));
                c.fx().indent += 1;
                c.fx().narrow.extend(pos);
                opened += 1;
            }
            c.inline_gens(&gens_v, i + 1, inner)?;
            for _ in 0..opened {
                c.fx().indent -= 1;
                c.emit("}");
            }
            Ok(())
        })
    }

    fn minmax(&mut self, is_min: bool, args: &[Expr], kws: &[Keyword], _want: Option<&Ty>, span: Span) -> R<CE> {
        let key = self.kw(kws, "key").cloned();
        let default = self.kw(kws, "default").cloned();
        let cmp = if is_min { "<" } else { ">" };
        if args.len() >= 2 {
            // min(a, b, ...)
            let mut ces = Vec::new();
            for a in args {
                ces.push(self.expr(a, None)?);
            }
            let mut t = ces[0].ty.clone();
            for c in &ces[1..] {
                t = self.unify(&t, &c.ty).ok_or_else(|| self.e(span, "min/max arguments have incompatible types"))?;
            }
            if key.is_some() {
                return Err(self.e(span, "key= with multiple positional arguments is not supported; pass a list"));
            }
            self.check_orderable(&t, span)?;
            let ct = self.ctype(&t);
            let r = self.fresh("mm");
            let mut code = format!("({{ {} {} = {}; ", ct, r, self.coerce(&ces[0].c, &ces[0].ty, &t));
            for c in &ces[1..] {
                let v = self.fresh("mv");
                let cc = self.coerce(&c.c, &c.ty, &t);
                let less = self.compare_one(CE::new(t.clone(), v.clone()), if is_min { CmpOp::Lt } else { CmpOp::Gt }, CE::new(t.clone(), r.clone()), span)?;
                code.push_str(&format!("{{ {} {} = {}; if ({}) {} = {}; }} ", ct, v, cc, less, r, v));
            }
            code.push_str(&format!("{}; }})", r));
            return Ok(CE::new(t, code));
        }
        let a0 = args.first().ok_or_else(|| self.e(span, "min/max expects an iterable"))?.clone();
        let et = self.iter_elem_type(&a0)?;
        let ct = self.ctype(&et);
        let best = self.fresh("best");
        let have = self.fresh("have");
        let (b2, h2) = (best.clone(), have.clone());
        let keyf = match &key {
            Some(k) => Some(self.key_func(k, &et)?),
            None => {
                self.check_orderable(&et, span)?;
                None
            }
        };
        let bestk = self.fresh("bk");
        let bk2 = bestk.clone();
        let et2 = et.clone();
        let kty = keyf.as_ref().map(|k| match &k.ty {
            Ty::Func(_, r) => (**r).clone(),
            _ => Ty::Any,
        });
        let kty2 = kty.clone();
        let kf2 = keyf.clone();
        let body = self.reduce_iter(&a0, &mut |c: &mut Compiler, item: CE| {
            let iv = c.fresh("iv");
            let ict = c.ctype(&et2);
            let ic = c.coerce(&item.c, &item.ty, &et2);
            c.emit(&format!("{{ {} {} = {};", ict, iv, ic));
            match (&kf2, &kty2) {
                (Some(kf), Some(kt)) => {
                    let kv = c.call_value(kf.clone(), vec![CE::new(et2.clone(), iv.clone())], span)?;
                    let kvn = c.fresh("kv");
                    let kct = c.ctype(kt);
                    c.emit(&format!("{} {} = {};", kct, kvn, kv.c));
                    let better = c.compare_one(CE::new(kt.clone(), kvn.clone()), if is_min { CmpOp::Lt } else { CmpOp::Gt }, CE::new(kt.clone(), bk2.clone()), span)?;
                    c.emit(&format!("if (!{} || {}) {{ {} = {}; {} = {}; {} = true; }} }}", h2, better, b2, iv, bk2, kvn, h2));
                }
                _ => {
                    let better = c.compare_one(CE::new(et2.clone(), iv.clone()), if is_min { CmpOp::Lt } else { CmpOp::Gt }, CE::new(et2.clone(), b2.clone()), span)?;
                    c.emit(&format!("if (!{} || {}) {{ {} = {}; {} = true; }} }}", h2, better, b2, iv, h2));
                }
            }
            Ok(())
        })?;
        let _ = cmp;
        let empty = match default {
            Some(d) => {
                let dc = self.expr(&d, Some(&et))?;
                let dc = self.check_coerce(&dc, &et, d.span)?;
                format!("if (!{}) {} = {};", have, best, dc)
            }
            None => format!("if (!{}) fpy_raise_code(FPY_EXC_VALUE, \"{}() arg is an empty sequence\");", have, if is_min { "min" } else { "max" }),
        };
        let kdecl = match &kty {
            Some(kt) => format!("{} {} = {};", self.ctype(kt), bestk, kt.zero()),
            None => String::new(),
        };
        Ok(CE::new(et.clone(), format!("({{ {} {} = {}; bool {} = false; {} {} {} {}; }})", ct, best, et.zero(), have, kdecl, body, empty, best)))
    }

    /// Compile a key function expression (lambda or function) for element type `et`.
    fn key_func(&mut self, k: &Expr, et: &Ty) -> R<CE> {
        let want = Ty::Func(vec![et.clone()], Box::new(Ty::Var("?".into())));
        let f = self.expr(k, Some(&want))?;
        match &f.ty {
            Ty::Func(ps, _) if ps.len() == 1 && self.assignable(et, &ps[0]) => Ok(f),
            t => Err(self.e(k.span, format!("key must be a function of one {} argument, got {}", self.tyname(et), self.tyname(t)))),
        }
    }

    /// C expression producing a list of keys for list variable `lst`.
    fn key_list(&mut self, k: &Expr, lst: &str, et: &Ty) -> R<String> {
        let f = self.key_func(k, et)?;
        let kt = match &f.ty {
            Ty::Func(_, r) => (**r).clone(),
            _ => unreachable!(),
        };
        self.check_orderable(&kt, k.span)?;
        let ect = self.ctype(et);
        let item = CE::new(et.clone(), et.from_word(&format!("{}->items[_ki]", lst), &ect));
        let call = self.call_value(CE::new(f.ty.clone(), "_kf".to_string()), vec![item], k.span)?;
        Ok(format!("({{ FpyFunc *_kf = {}; FpyList *_kl = fpy_list_new({}, {}->len); for (int64_t _ki = 0; _ki < {}->len; _ki++) fpy_list_append(_kl, {}); _kl; }})", f.c, kt.kind(), lst, lst, kt.to_word(&call.c)))
    }

    // ------------------------------------------------------------------ builtin type methods

    fn builtin_method(&mut self, recv: CE, m: &str, args: &[Expr], kws: &[Keyword], want: Option<&Ty>, span: Span) -> R<CE> {
        let n = args.len();
        let rt = recv.ty.clone();
        let r = recv.c.clone();
        macro_rules! s_arg {
            ($i:expr) => {{
                let ce = self.arg(args, $i, Some(&Ty::Str), m, span)?;
                self.check_coerce(&ce, &Ty::Str, span)?
            }};
        }
        macro_rules! opt_s {
            ($i:expr) => {{
                match args.get($i) {
                    Some(a) if !matches!(a.kind, ExprKind::NoneLit) => {
                        let ce = self.expr(a, Some(&Ty::Str))?;
                        self.check_coerce(&ce, &Ty::Str, a.span)?
                    }
                    _ => "NULL".to_string(),
                }
            }};
        }
        macro_rules! i_arg {
            ($i:expr, $d:expr) => {{
                match args.get($i) {
                    Some(a) => self.expect_int(a)?,
                    None => $d.to_string(),
                }
            }};
        }
        match &rt {
            Ty::Str => {
                let res = |t: Ty, c: String| Ok(CE::new(t, c));
                match m {
                    "upper" | "lower" | "capitalize" | "title" | "swapcase" => res(Ty::Str, format!("fpy_str_{}({})", m, r)),
                    "casefold" => res(Ty::Str, format!("fpy_str_lower({})", r)),
                    "strip" | "lstrip" | "rstrip" => {
                        let c = opt_s!(0);
                        res(Ty::Str, format!("fpy_str_{}({}, {})", m, r, c))
                    }
                    "split" | "rsplit" => {
                        let sep = match args.first().or(self.kw(kws, "sep")) {
                            Some(a) if !matches!(a.kind, ExprKind::NoneLit) => {
                                let ce = self.expr(a, Some(&Ty::Str))?;
                                self.check_coerce(&ce, &Ty::Str, a.span)?
                            }
                            _ => "NULL".to_string(),
                        };
                        let ms = match args.get(1).or(self.kw(kws, "maxsplit")) {
                            Some(a) => self.expect_int(a)?,
                            None => "-1".into(),
                        };
                        res(Ty::list(Ty::Str), format!("fpy_str_{}({}, {}, {})", m, r, sep, ms))
                    }
                    "splitlines" => {
                        let keep = match args.first().or(self.kw(kws, "keepends")) {
                            Some(a) => {
                                let ce = self.expr(a, Some(&Ty::Bool))?;
                                self.truthy(&ce)?
                            }
                            None => "false".into(),
                        };
                        res(Ty::list(Ty::Str), format!("fpy_str_splitlines({}, {})", r, keep))
                    }
                    "join" => {
                        let lst = self.to_list(&args[0], Some(Ty::Str))?;
                        if lst.ty != Ty::list(Ty::Str) {
                            return Err(self.e(span, format!("str.join() needs an iterable of str, got {}", self.tyname(&lst.ty))));
                        }
                        res(Ty::Str, format!("fpy_str_join({}, {})", r, lst.c))
                    }
                    "replace" => {
                        let a = s_arg!(0);
                        let b = s_arg!(1);
                        let c = i_arg!(2, "-1");
                        res(Ty::Str, format!("fpy_str_replace({}, {}, {}, {})", r, a, b, c))
                    }
                    "startswith" | "endswith" => {
                        if let Some(ExprKind::Tuple { elts, .. }) = args.first().map(|a| &a.kind) {
                            let t = self.fresh("sw");
                            let mut conds = Vec::new();
                            for e in elts {
                                let ce = self.expr(e, Some(&Ty::Str))?;
                                let c = self.check_coerce(&ce, &Ty::Str, e.span)?;
                                conds.push(format!("fpy_str_{}({}, {})", m, t, c));
                            }
                            return res(Ty::Bool, format!("({{ FpyStr *{} = {}; {}; }})", t, r, conds.join(" || ")));
                        }
                        let a = s_arg!(0);
                        if n > 1 {
                            let st = i_arg!(1, "0");
                            return res(Ty::Bool, format!("fpy_str_{}(fpy_str_slice({}, true, {}, false, 0, 1), {})", m, r, st, a));
                        }
                        res(Ty::Bool, format!("fpy_str_{}({}, {})", m, r, a))
                    }
                    "find" | "rfind" | "index" | "count" => {
                        let a = s_arg!(0);
                        if n > 1 {
                            return Err(self.e(span, format!("str.{}() with start/end is not supported; slice first", m)));
                        }
                        res(Ty::Int, format!("fpy_str_{}({}, {})", m, r, a))
                    }
                    "rindex" => {
                        let a = s_arg!(0);
                        res(Ty::Int, format!("({{ fpy_int _x = fpy_str_rfind({}, {}); if (_x < 0) fpy_raise_code(FPY_EXC_VALUE, \"substring not found\"); _x; }})", r, a))
                    }
                    "isdigit" | "isalpha" | "isalnum" | "isspace" | "isupper" | "islower" | "isidentifier" | "istitle" | "isascii" | "isprintable" => res(Ty::Bool, format!("fpy_str_{}({})", m, r)),
                    "isnumeric" | "isdecimal" => res(Ty::Bool, format!("fpy_str_isdigit({})", r)),
                    "center" | "ljust" | "rjust" => {
                        let w = i_arg!(0, "0");
                        let f = opt_s!(1);
                        res(Ty::Str, format!("fpy_str_{}({}, {}, {})", m, r, w, f))
                    }
                    "zfill" => {
                        let w = i_arg!(0, "0");
                        res(Ty::Str, format!("fpy_str_zfill({}, {})", r, w))
                    }
                    "removeprefix" | "removesuffix" => {
                        let a = s_arg!(0);
                        res(Ty::Str, format!("fpy_str_{}({}, {})", m, r, a))
                    }
                    "partition" | "rpartition" => {
                        let a = s_arg!(0);
                        res(Ty::Tuple(vec![Ty::Str, Ty::Str, Ty::Str]), format!("fpy_str_{}({}, {})", m, r, a))
                    }
                    "encode" => res(Ty::Bytes, format!("fpy_str_encode({})", r)),
                    "format" => Err(self.e(span, "str.format() is not supported; use an f-string")),
                    "expandtabs" => {
                        let ts = i_arg!(0, "8");
                        res(Ty::Str, format!("fpy_str_expandtabs({}, {})", r, ts))
                    }
                    _ => Err(self.e(span, format!("'str' object has no method '{}'", m))),
                }
            }
            Ty::List(et) => {
                let et = (**et).clone();
                let ect = self.ctype(&et);
                match m {
                    "append" => {
                        let v = self.arg(args, 0, Some(&et), m, span)?;
                        let c = self.check_coerce(&v, &et, span)?;
                        Ok(CE::new(Ty::NoneT, format!("fpy_list_append({}, {})", r, et.to_word(&c))))
                    }
                    "extend" => {
                        let v = self.arg(args, 0, Some(&rt), m, span)?;
                        match &v.ty {
                            Ty::List(_) => {
                                let c = self.check_coerce(&v, &rt, span)?;
                                Ok(CE::new(Ty::NoneT, format!("fpy_list_extend({}, {})", r, c)))
                            }
                            _ => {
                                let c = self.check_coerce(&v, &Ty::iter(et.clone()), span)?;
                                Ok(CE::new(Ty::NoneT, format!("fpy_list_extend_iter({}, {})", r, c)))
                            }
                        }
                    }
                    "insert" => {
                        let i = i_arg!(0, "0");
                        let v = self.arg(args, 1, Some(&et), m, span)?;
                        let c = self.check_coerce(&v, &et, span)?;
                        Ok(CE::new(Ty::NoneT, format!("fpy_list_insert({}, {}, {})", r, i, et.to_word(&c))))
                    }
                    "pop" => {
                        let i = i_arg!(0, "-1");
                        Ok(CE::new(et.clone(), et.from_word(&format!("fpy_list_pop({}, {})", r, i), &ect)))
                    }
                    "remove" | "index" | "count" => {
                        let v = self.arg(args, 0, Some(&et), m, span)?;
                        let c = self.check_coerce(&v, &et, span)?;
                        let t = if m == "remove" { Ty::NoneT } else { Ty::Int };
                        Ok(CE::new(t, format!("fpy_list_{}({}, {})", m, r, et.to_word(&c))))
                    }
                    "reverse" | "clear" => Ok(CE::new(Ty::NoneT, format!("fpy_list_{}({})", m, r))),
                    "copy" => Ok(CE::new(rt.clone(), format!("fpy_list_copy({})", r))),
                    "sort" => {
                        let rev = match self.kw(kws, "reverse") {
                            Some(x) => {
                                let ce = self.expr(x, Some(&Ty::Bool))?;
                                self.truthy(&ce)?
                            }
                            None => "false".into(),
                        };
                        match self.kw(kws, "key").cloned() {
                            Some(k) => {
                                let lt = self.fresh("sl");
                                let keys = self.key_list(&k, &lt, &et)?;
                                Ok(CE::new(Ty::NoneT, format!("({{ FpyList *{lt} = {}; if ({lt}->len > 1) fpy_list_sort_by({lt}, {}, {}); }})", r, keys, rev)))
                            }
                            None => {
                                self.check_orderable(&et, span)?;
                                Ok(CE::new(Ty::NoneT, format!("fpy_list_sort({}, {})", r, rev)))
                            }
                        }
                    }
                    _ => Err(self.e(span, format!("'list' object has no method '{}'", m))),
                }
            }
            Ty::Dict(kt, vt) => {
                let (kt, vt) = ((**kt).clone(), (**vt).clone());
                let vct = self.ctype(&vt);
                let kct = self.ctype(&kt);
                match m {
                    "get" => {
                        let k = self.arg(args, 0, Some(&kt), m, span)?;
                        let kc = self.check_coerce(&k, &kt, span)?;
                        let kw = kt.to_word(&kc);
                        match args.get(1) {
                            Some(d) => {
                                let dc = self.expr(d, Some(&vt))?;
                                let dc = self.check_coerce(&dc, &vt, d.span)?;
                                Ok(CE::new(vt.clone(), format!("({{ fpy_word _v; fpy_dict_lookup({}, {}, &_v) ? {} : {}; }})", r, kw, vt.from_word("_v", &vct), dc)))
                            }
                            None => {
                                let ot = Ty::optional(vt.clone());
                                let some = self.coerce(&vt.from_word("_v", &vct), &vt, &ot);
                                let oct = self.ctype(&ot);
                                Ok(CE::new(ot, format!("({{ fpy_word _v; fpy_dict_lookup({}, {}, &_v) ? {} : ({})NULL; }})", r, kw, some, oct)))
                            }
                        }
                    }
                    "keys" => Ok(CE::new(Ty::list(kt.clone()), format!("fpy_dict_keys({})", r))),
                    "values" => Ok(CE::new(Ty::list(vt.clone()), format!("fpy_dict_values({})", r))),
                    "items" => Ok(CE::new(Ty::list(Ty::Tuple(vec![kt.clone(), vt.clone()])), format!("fpy_dict_items({})", r))),
                    "pop" => {
                        let k = self.arg(args, 0, Some(&kt), m, span)?;
                        let kc = self.check_coerce(&k, &kt, span)?;
                        let (hd, d) = match args.get(1) {
                            Some(d) => {
                                let dc = self.expr(d, Some(&vt))?;
                                let dc = self.check_coerce(&dc, &vt, d.span)?;
                                ("true", vt.to_word(&dc))
                            }
                            None => ("false", "0".to_string()),
                        };
                        Ok(CE::new(vt.clone(), vt.from_word(&format!("fpy_dict_pop({}, {}, {}, {})", r, kt.to_word(&kc), hd, d), &vct)))
                    }
                    "setdefault" => {
                        let k = self.arg(args, 0, Some(&kt), m, span)?;
                        let kc = self.check_coerce(&k, &kt, span)?;
                        let d = self.arg(args, 1, Some(&vt), m, span)?;
                        let dc = self.check_coerce(&d, &vt, span)?;
                        Ok(CE::new(vt.clone(), vt.from_word(&format!("fpy_dict_setdefault({}, {}, {})", r, kt.to_word(&kc), vt.to_word(&dc)), &vct)))
                    }
                    "update" => {
                        let o = self.arg(args, 0, Some(&rt), m, span)?;
                        let oc = self.check_coerce(&o, &rt, span)?;
                        Ok(CE::new(Ty::NoneT, format!("fpy_dict_update({}, {})", r, oc)))
                    }
                    "copy" => Ok(CE::new(rt.clone(), format!("fpy_dict_copy({})", r))),
                    "clear" => Ok(CE::new(Ty::NoneT, format!("fpy_dict_clear({})", r))),
                    "popitem" => {
                        let _ = kct;
                        Ok(CE::new(Ty::Tuple(vec![kt.clone(), vt.clone()]), format!("fpy_dict_popitem({})", r)))
                    }
                    _ => Err(self.e(span, format!("'dict' object has no method '{}'", m))),
                }
            }
            Ty::Set(et) => {
                let et = (**et).clone();
                let ect = self.ctype(&et);
                match m {
                    "add" | "remove" | "discard" => {
                        let v = self.arg(args, 0, Some(&et), m, span)?;
                        let c = self.check_coerce(&v, &et, span)?;
                        Ok(CE::new(Ty::NoneT, format!("fpy_set_{}({}, {})", m, r, et.to_word(&c))))
                    }
                    "pop" => Ok(CE::new(et.clone(), et.from_word(&format!("fpy_set_pop({})", r), &ect))),
                    "union" | "intersection" | "difference" | "symmetric_difference" => {
                        let o = self.set_arg(args, 0, &et, span)?;
                        let f = if m == "symmetric_difference" { "symdiff" } else { m };
                        Ok(CE::new(rt.clone(), format!("fpy_set_{}({}, {})", f, r, o)))
                    }
                    "update" | "intersection_update" | "difference_update" => {
                        let o = self.set_arg(args, 0, &et, span)?;
                        let f = match m {
                            "update" => "union",
                            "intersection_update" => "intersection",
                            _ => "difference",
                        };
                        Ok(CE::new(Ty::NoneT, format!("({{ FpyDict *_s = {}; FpyDict *_r = fpy_set_{}(_s, {}); fpy_dict_clear(_s); fpy_dict_update(_s, _r); }})", r, f, o)))
                    }
                    "issubset" => {
                        let o = self.set_arg(args, 0, &et, span)?;
                        Ok(CE::new(Ty::Bool, format!("fpy_set_issubset({}, {})", r, o)))
                    }
                    "issuperset" => {
                        let o = self.set_arg(args, 0, &et, span)?;
                        Ok(CE::new(Ty::Bool, format!("fpy_set_issubset({}, {})", o, r)))
                    }
                    "isdisjoint" => {
                        let o = self.set_arg(args, 0, &et, span)?;
                        Ok(CE::new(Ty::Bool, format!("(fpy_set_intersection({}, {})->len == 0)", r, o)))
                    }
                    "copy" => Ok(CE::new(rt.clone(), format!("fpy_dict_copy({})", r))),
                    "clear" => Ok(CE::new(Ty::NoneT, format!("fpy_dict_clear({})", r))),
                    _ => Err(self.e(span, format!("'set' object has no method '{}'", m))),
                }
            }
            Ty::Bytes => match m {
                "decode" => Ok(CE::new(Ty::Str, format!("fpy_bytes_decode({})", r))),
                "hex" => Ok(CE::new(Ty::Str, format!("({{ FpyBytes *_b = {}; FpySB _sb; fpy_sb_init(&_sb); char _h[3]; for (int64_t _i = 0; _i < _b->len; _i++) {{ snprintf(_h, 3, \"%02x\", _b->data[_i]); fpy_sb_add(&_sb, _h, 2); }} fpy_sb_finish(&_sb); }})", r))),
                _ => Err(self.e(span, format!("'bytes' object has no method '{}'", m))),
            },
            Ty::Int => match m {
                "bit_length" => Ok(CE::new(Ty::Int, format!("({{ fpy_int _v = {}; _v == 0 ? 0 : (64 - __builtin_clzll((uint64_t)(_v < 0 ? -_v : _v))); }})", r))),
                "bit_count" => Ok(CE::new(Ty::Int, format!("((fpy_int)__builtin_popcountll((uint64_t)fpy_iabs({})))", r))),
                _ => Err(self.e(span, format!("'int' object has no method '{}'", m))),
            },
            Ty::Float => match m {
                "is_integer" => Ok(CE::new(Ty::Bool, format!("({{ double _v = {}; isfinite(_v) && _v == floor(_v); }})", r))),
                "hex" => Ok(CE::new(Ty::Str, format!("fpy_float_hex({})", r))),
                _ => Err(self.e(span, format!("'float' object has no method '{}'", m))),
            },
            Ty::Tuple(ts) => match m {
                "count" | "index" if !ts.is_empty() && ts.iter().all(|t| *t == ts[0]) => {
                    let et = ts[0].clone();
                    let v = self.arg(args, 0, Some(&et), m, span)?;
                    let c = self.check_coerce(&v, &et, span)?;
                    let lst = format!("({{ FpyTuple *_t = {}; fpy_list_from({}, _t->len, _t->items); }})", r, et.kind());
                    Ok(CE::new(Ty::Int, format!("fpy_list_{}({}, {})", m, lst, et.to_word(&c))))
                }
                _ => Err(self.e(span, format!("'tuple' object has no method '{}'", m))),
            },
            Ty::VarTuple(et) => match m {
                "count" | "index" => {
                    let et = (**et).clone();
                    let v = self.arg(args, 0, Some(&et), m, span)?;
                    let c = self.check_coerce(&v, &et, span)?;
                    let lst = format!("({{ FpyTuple *_t = {}; fpy_list_from({}, _t->len, _t->items); }})", r, et.kind());
                    Ok(CE::new(Ty::Int, format!("fpy_list_{}({}, {})", m, lst, et.to_word(&c))))
                }
                _ => Err(self.e(span, format!("'tuple' object has no method '{}'", m))),
            },
            Ty::Iter(et) => match m {
                "__next__" => {
                    let et = (**et).clone();
                    let ct = self.ctype(&et);
                    Ok(CE::new(et.clone(), format!("({{ FpyIter *_i = {}; fpy_word _w; if (!_i->next(_i, &_w)) fpy_raise_stop_iteration(); {}; }})", r, et.from_word("_w", &ct))))
                }
                // generator methods: the sent value travels boxed (the generator checks its type)
                "send" => {
                    let et = (**et).clone();
                    let ct = self.ctype(&et);
                    let v = self.arg(args, 0, None, m, span)?;
                    let boxed = self.coerce(&v.c, &v.ty, &Ty::Any);
                    Ok(CE::new(et.clone(), et.from_word(&format!("fpy_gen_send({}, {})", r, boxed), &ct)))
                }
                "throw" => {
                    let et = (**et).clone();
                    let ct = self.ctype(&et);
                    let v = self.arg(args, 0, None, m, span)?;
                    let exc = match &v.ty {
                        Ty::Class(c) if self.is_exception_class(*c) => format!("(FpyObj*)({})", v.c),
                        Ty::Type(c) if self.is_exception_class(*c) => {
                            let cn = self.classes[*c].cname.clone();
                            format!("(FpyObj*)T_{}.ctor(NULL, NULL, 0)", cn)
                        }
                        t => return Err(self.e(span, format!("throw() needs an exception, got {}", self.tyname(t)))),
                    };
                    Ok(CE::new(et.clone(), et.from_word(&format!("fpy_gen_throw({}, {})", r, exc), &ct)))
                }
                "close" => Ok(CE::new(Ty::NoneT, format!("(fpy_gen_close({}), (FpyObj*)NULL)", r))),
                _ => Err(self.e(span, format!("iterator has no method '{}'", m))),
            },
            Ty::Func(..) => {
                let _ = want;
                Err(self.e(span, format!("function has no method '{}'", m)))
            }
            t => Err(self.e(span, format!("'{}' has no method '{}'", self.tyname(t), m))),
        }
    }

    fn set_arg(&mut self, args: &[Expr], i: usize, et: &Ty, span: Span) -> R<String> {
        let a = args.get(i).ok_or_else(|| self.e(span, "missing argument"))?;
        let v = self.expr(a, Some(&Ty::set(et.clone())))?;
        match &v.ty {
            Ty::Set(e) if **e == *et => Ok(v.c),
            t => {
                let e2 = self.elem_type(t).ok_or_else(|| self.e(span, "expected an iterable"))?;
                if e2 != *et {
                    return Err(self.e(span, format!("expected set[{}]", self.tyname(et))));
                }
                let ic = self.iter_of(&v, &e2);
                Ok(format!("fpy_set_from_iter({}, {})", et.kind(), ic))
            }
        }
    }

    pub fn loop_elem_type_pub(&mut self, iter: &Expr, ty: &Ty) -> Option<Ty> {
        // same as expr::loop_elem_type (kept private there)
        if let ExprKind::Call { func, args, .. } = &iter.kind {
            if let ExprKind::Name(n) = &func.kind {
                if self.is_builtin_name(n) {
                    match n.as_str() {
                        "range" => return Some(Ty::Int),
                        "enumerate" if !args.is_empty() => {
                            let saved = self.fxr().dry_run;
                            self.fx().dry_run = true;
                            let r = self.expr(&args[0], None);
                            self.fx().dry_run = saved;
                            let inner = self.elem_type(&r.ok()?.ty)?;
                            return Some(Ty::Tuple(vec![Ty::Int, inner]));
                        }
                        "zip" => {
                            let mut ts = Vec::new();
                            for a in args {
                                let saved = self.fxr().dry_run;
                                self.fx().dry_run = true;
                                let r = self.expr(a, None);
                                self.fx().dry_run = saved;
                                ts.push(self.elem_type(&r.ok()?.ty)?);
                            }
                            return Some(Ty::Tuple(ts));
                        }
                        _ => {}
                    }
                }
            }
        }
        self.elem_type(ty)
    }

    pub fn declare_comp_targets_pub(&mut self, t: &Expr, et: &Ty) -> R<()> {
        match &t.kind {
            ExprKind::Name(n) => {
                let cname = format!("cv_{}_{}", c_ident(n), self.tmp);
                self.tmp += 1;
                self.fx().comp.last_mut().unwrap().insert(n.clone(), Local { ty: et.clone(), cname, cell: false });
                Ok(())
            }
            ExprKind::Tuple { elts, .. } | ExprKind::List(elts) => {
                let ets: Vec<Ty> = match et {
                    Ty::Tuple(ts) if ts.len() == elts.len() => ts.clone(),
                    Ty::List(e) => vec![(**e).clone(); elts.len()],
                    Ty::Any => vec![Ty::Any; elts.len()],
                    _ => return Err(self.e(t.span, format!("cannot unpack {}", self.tyname(et)))),
                };
                for (e, t2) in elts.iter().zip(ets) {
                    self.declare_comp_targets_pub(e, &t2)?;
                }
                Ok(())
            }
            _ => Err(self.e(t.span, "unsupported comprehension target")),
        }
    }
}

fn elem_of(t: &Ty) -> Option<Ty> {
    match t {
        Ty::List(e) | Ty::Set(e) | Ty::Iter(e) | Ty::VarTuple(e) => Some((**e).clone()),
        Ty::Dict(k, _) => Some((**k).clone()),
        Ty::Str => Some(Ty::Str),
        Ty::Bytes => Some(Ty::Int),
        Ty::Tuple(ts) if !ts.is_empty() && ts.iter().all(|x| *x == ts[0]) => Some(ts[0].clone()),
        _ => None,
    }
}
