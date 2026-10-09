/* fpython runtime — support for pure-Python modules compiled to native code (FPY_PYTHON only).
 *
 * `fpython` compiles `.py` modules to C that drives the CPython object model directly (no bytecode
 * interpretation). Compiled modules are registered with a meta-path finder; anything the compiler
 * cannot handle is imported normally by CPython.
 */
#ifdef FPY_PYTHON
#define PY_SSIZE_T_CLEAN
#include <Python.h>

#include "fpy_pyc_api.h"

/* A compiled function object: vectorcall + descriptor binding + attribute dict. */
typedef struct {
    PyObject_HEAD
    vectorcallfunc vectorcall;
    const FpcSpec *spec;
    PyObject *defaults;   /* tuple or NULL */
    PyObject *kwdefaults; /* dict or NULL */
    PyObject *closure;    /* tuple of cells or NULL */
    PyObject *globals;    /* module dict */
    PyObject *dict;
} FpcFunction;

static PyObject *fpc_function_type;
static PyObject *fpc_annotations(void *f);

static int fpc_bind(FpcFunction *f, PyObject *const *args, size_t nargsf, PyObject *kwnames, PyObject **slots) {
    const FpcSpec *s = f->spec;
    Py_ssize_t nargs = PyVectorcall_NARGS(nargsf);
    int nslots = s->npos + s->nkwonly + s->has_varargs + s->has_varkw;
    for (int i = 0; i < nslots; i++) slots[i] = NULL;
    Py_ssize_t npos_given = nargs < s->npos ? nargs : s->npos;
    for (Py_ssize_t i = 0; i < npos_given; i++) slots[i] = Py_NewRef(args[i]);
    int vi = s->npos + s->nkwonly;
    if (nargs > s->npos) {
        if (!s->has_varargs) {
            PyErr_Format(PyExc_TypeError, "%s() takes %d positional arguments but %zd were given", s->name, s->npos, nargs);
            goto fail;
        }
        slots[vi] = PyTuple_New(nargs - s->npos);
        for (Py_ssize_t i = s->npos; i < nargs; i++) PyTuple_SET_ITEM(slots[vi], i - s->npos, Py_NewRef(args[i]));
    } else if (s->has_varargs) {
        slots[vi] = PyTuple_New(0);
    }
    int ki = vi + s->has_varargs;
    if (s->has_varkw) slots[ki] = PyDict_New();
    if (kwnames) {
        Py_ssize_t nk = PyTuple_GET_SIZE(kwnames);
        for (Py_ssize_t k = 0; k < nk; k++) {
            PyObject *key = PyTuple_GET_ITEM(kwnames, k);
            PyObject *val = args[nargs + k];
            int found = -1;
            for (int i = s->nposonly; i < s->npos + s->nkwonly; i++)
                if (PyUnicode_EqualToUTF8(key, s->params[i])) { found = i; break; }
            if (found < 0) {
                if (!s->has_varkw) {
                    PyErr_Format(PyExc_TypeError, "%s() got an unexpected keyword argument '%U'", s->name, key);
                    goto fail;
                }
                if (PyDict_SetItem(slots[ki], key, val) < 0) goto fail;
                continue;
            }
            if (slots[found]) {
                PyErr_Format(PyExc_TypeError, "%s() got multiple values for argument '%s'", s->name, s->params[found]);
                goto fail;
            }
            slots[found] = Py_NewRef(val);
        }
    }
    /* defaults */
    Py_ssize_t nd = f->defaults ? PyTuple_GET_SIZE(f->defaults) : 0;
    for (int i = 0; i < s->npos; i++) {
        if (slots[i]) continue;
        Py_ssize_t di = i - (s->npos - nd);
        if (di >= 0) slots[i] = Py_NewRef(PyTuple_GET_ITEM(f->defaults, di));
        else {
            PyErr_Format(PyExc_TypeError, "%s() missing required positional argument: '%s'", s->name, s->params[i]);
            goto fail;
        }
    }
    for (int i = s->npos; i < s->npos + s->nkwonly; i++) {
        if (slots[i]) continue;
        PyObject *d = f->kwdefaults ? PyDict_GetItemString(f->kwdefaults, s->params[i]) : NULL;
        if (d) slots[i] = Py_NewRef(d);
        else {
            PyErr_Format(PyExc_TypeError, "%s() missing required keyword-only argument: '%s'", s->name, s->params[i]);
            goto fail;
        }
    }
    return 0;
fail:
    for (int i = 0; i < nslots; i++) Py_CLEAR(slots[i]);
    return -1;
}

static PyObject *fpc_vectorcall(PyObject *callable, PyObject *const *args, size_t nargsf, PyObject *kwnames) {
    FpcFunction *f = (FpcFunction *)callable;
    PyObject *slots[64];
    int nslots = f->spec->npos + f->spec->nkwonly + f->spec->has_varargs + f->spec->has_varkw;
    if (nslots > 64) { PyErr_SetString(PyExc_SystemError, "too many parameters"); return NULL; }
    if (fpc_bind(f, args, nargsf, kwnames, slots) < 0) return NULL;
    /* the impl takes ownership of the slot references */
    return f->spec->impl(callable, slots);
}

static PyObject *fpc_descr_get(PyObject *self, PyObject *obj, PyObject *type) {
    (void)type;
    if (!obj || obj == Py_None) return Py_NewRef(self);
    return PyMethod_New(self, obj);
}

static void fpc_dealloc(PyObject *self) {
    FpcFunction *f = (FpcFunction *)self;
    PyObject_GC_UnTrack(self);
    Py_CLEAR(f->defaults);
    Py_CLEAR(f->kwdefaults);
    Py_CLEAR(f->closure);
    Py_CLEAR(f->globals);
    Py_CLEAR(f->dict);
    Py_TYPE(self)->tp_free(self);
}

static int fpc_traverse(PyObject *self, visitproc visit, void *arg) {
    FpcFunction *f = (FpcFunction *)self;
    Py_VISIT(f->defaults);
    Py_VISIT(f->kwdefaults);
    Py_VISIT(f->closure);
    Py_VISIT(f->globals);
    Py_VISIT(f->dict);
    Py_VISIT(Py_TYPE(self));
    return 0;
}

static int fpc_clear(PyObject *self) {
    FpcFunction *f = (FpcFunction *)self;
    Py_CLEAR(f->defaults);
    Py_CLEAR(f->kwdefaults);
    Py_CLEAR(f->closure);
    Py_CLEAR(f->dict);
    return 0;
}

static PyObject *sig_helper;

static PyObject *fpc_annotations(void *fp) {
    FpcFunction *f = (FpcFunction *)fp;
    PyObject *d = PyDict_New();
    const FpcSpec *s = f->spec;
    int n = s->npos + s->nkwonly + s->has_varargs + s->has_varkw;
    for (int i = 0; i < n; i++)
        if (s->annots && s->annots[i]) {
            PyObject *v = PyUnicode_FromString(s->annots[i]);
            PyDict_SetItemString(d, s->params[i], v);
            Py_DECREF(v);
        }
    if (s->ret_annot) {
        PyObject *v = PyUnicode_FromString(s->ret_annot);
        PyDict_SetItemString(d, "return", v);
        Py_DECREF(v);
    }
    return d;
}

static PyObject *fpc_get_signature(PyObject *self, void *closure) {
    (void)closure;
    FpcFunction *f = (FpcFunction *)self;
    PyObject *cached = PyDict_GetItemString(f->dict, "__signature__");
    if (cached) return Py_NewRef(cached);
    if (!sig_helper) {
        static const char *SRC =
            "import inspect\n"
            "P = inspect.Parameter\n"
            "def sig(names, nposonly, npos, nkwonly, var, kw, defaults, kwdefaults, annots):\n"
            "    ps = []\n"
            "    defaults = defaults or (); kwdefaults = kwdefaults or {}\n"
            "    nd = len(defaults)\n"
            "    for i in range(npos):\n"
            "        d = defaults[i - (npos - nd)] if i >= npos - nd else P.empty\n"
            "        kind = P.POSITIONAL_ONLY if i < nposonly else P.POSITIONAL_OR_KEYWORD\n"
            "        ps.append(P(names[i], kind, default=d, annotation=annots.get(names[i], P.empty)))\n"
            "    j = npos + nkwonly\n"
            "    if var: ps.append(P(names[j], P.VAR_POSITIONAL, annotation=annots.get(names[j], P.empty))); j += 1\n"
            "    for i in range(npos, npos + nkwonly):\n"
            "        ps.append(P(names[i], P.KEYWORD_ONLY, default=kwdefaults.get(names[i], P.empty), annotation=annots.get(names[i], P.empty)))\n"
            "    if kw: ps.append(P(names[j], P.VAR_KEYWORD, annotation=annots.get(names[j], P.empty)))\n"
            "    return inspect.Signature(ps, return_annotation=annots.get('return', inspect.Signature.empty))\n";
        PyObject *g = PyDict_New();
        PyDict_SetItemString(g, "__builtins__", PyEval_GetBuiltins());
        PyObject *r = PyRun_String(SRC, Py_file_input, g, g);
        if (!r) { Py_DECREF(g); return NULL; }
        Py_DECREF(r);
        sig_helper = Py_NewRef(PyDict_GetItemString(g, "sig"));
        Py_DECREF(g);
    }
    const FpcSpec *s = f->spec;
    int n = s->npos + s->nkwonly + s->has_varargs + s->has_varkw;
    PyObject *names = PyTuple_New(n);
    /* params[] order: positional, kw-only, *args, **kw */
    for (int i = 0; i < n; i++) PyTuple_SET_ITEM(names, i, PyUnicode_FromString(s->params[i]));
    /* the helper expects *args right after kw-only names (same layout) */
    PyObject *ann = PyDict_GetItemString(f->dict, "__annotations__");
    PyObject *annots = ann ? Py_NewRef(ann) : fpc_annotations(f);
    PyObject *r = PyObject_CallFunction(sig_helper, "(NiiiiiOOO)", names, s->nposonly, s->npos, s->nkwonly, s->has_varargs, s->has_varkw,
                                        f->defaults ? f->defaults : Py_None, f->kwdefaults ? f->kwdefaults : Py_None, annots);
    Py_DECREF(annots);
    return r;
}

static int fpc_set_signature(PyObject *self, PyObject *v, void *closure) {
    (void)closure;
    FpcFunction *f = (FpcFunction *)self;
    if (!v) return PyDict_DelItemString(f->dict, "__signature__");
    return PyDict_SetItemString(f->dict, "__signature__", v);
}

static PyObject *fpc_get_annotations(PyObject *self, void *closure) {
    (void)closure;
    FpcFunction *f = (FpcFunction *)self;
    PyObject *a = PyDict_GetItemString(f->dict, "__annotations__");
    if (a) return Py_NewRef(a);
    a = fpc_annotations(f);
    PyDict_SetItemString(f->dict, "__annotations__", a);
    return a;
}

static int fpc_set_annotations(PyObject *self, PyObject *v, void *closure) {
    (void)closure;
    FpcFunction *f = (FpcFunction *)self;
    if (!v) return PyDict_DelItemString(f->dict, "__annotations__");
    return PyDict_SetItemString(f->dict, "__annotations__", v);
}

static PyGetSetDef fpc_getset[] = {
    {"__signature__", fpc_get_signature, fpc_set_signature, NULL, NULL},
    {"__annotations__", fpc_get_annotations, fpc_set_annotations, NULL, NULL},
    {NULL},
};

static PyObject *fpc_repr(PyObject *self) {
    return PyUnicode_FromFormat("<compiled function %s at %p>", ((FpcFunction *)self)->spec->qualname, self);
}

static PyMemberDef fpc_members[] = {
    {"__dictoffset__", Py_T_PYSSIZET, offsetof(FpcFunction, dict), Py_READONLY},
    {"__vectorcalloffset__", Py_T_PYSSIZET, offsetof(FpcFunction, vectorcall), Py_READONLY},
    {"__defaults__", Py_T_OBJECT_EX, offsetof(FpcFunction, defaults), 0},
    {"__kwdefaults__", Py_T_OBJECT_EX, offsetof(FpcFunction, kwdefaults), 0},
    {"__globals__", Py_T_OBJECT_EX, offsetof(FpcFunction, globals), Py_READONLY},
    {NULL},
};

static PyType_Slot fpc_slots[] = {
    {Py_tp_members, fpc_members},
    {Py_tp_getset, fpc_getset},
    {Py_tp_call, PyVectorcall_Call},
    {Py_tp_descr_get, fpc_descr_get},
    {Py_tp_dealloc, fpc_dealloc},
    {Py_tp_traverse, fpc_traverse},
    {Py_tp_clear, fpc_clear},
    {Py_tp_repr, fpc_repr},
    {0, NULL},
};

static PyType_Spec fpc_spec = {
    .name = "fpython.CompiledFunction",
    .basicsize = sizeof(FpcFunction),
    .flags = Py_TPFLAGS_DEFAULT | Py_TPFLAGS_HAVE_GC | Py_TPFLAGS_HAVE_VECTORCALL | Py_TPFLAGS_METHOD_DESCRIPTOR,
    .slots = fpc_slots,
};

static PyObject *native_function_new(const FpcSpec *spec, PyObject *globals, PyObject *defaults, PyObject *kwdefaults, PyObject *closure, PyObject *doc) {
    if (!fpc_function_type) {
        fpc_function_type = PyType_FromSpec(&fpc_spec);
        if (!fpc_function_type) return NULL;
    }
    FpcFunction *f = PyObject_GC_New(FpcFunction, (PyTypeObject *)fpc_function_type);
    if (!f) return NULL;
    f->vectorcall = fpc_vectorcall;
    f->spec = spec;
    f->defaults = defaults && defaults != Py_None ? Py_NewRef(defaults) : NULL;
    f->kwdefaults = kwdefaults && kwdefaults != Py_None ? Py_NewRef(kwdefaults) : NULL;
    f->closure = closure ? Py_NewRef(closure) : NULL;
    f->globals = Py_XNewRef(globals);
    f->dict = PyDict_New();
    (void)doc;
    PyObject_GC_Track((PyObject *)f);
    return (PyObject *)f;
}

/* Python-visible functions are real `function` objects: a trampoline with the original signature
 * (one bytecode CALL) whose body is the native implementation. This keeps every introspection
 * API (inspect.signature, __defaults__, __globals__, isfunction, method binding) working. */
static PyObject *tramp_helper, *tramp_async_helper;

PyObject *fpc_make_function(const FpcSpec *spec, PyObject *globals, PyObject *defaults, PyObject *kwdefaults, PyObject *closure, PyObject *doc) {
    PyObject *nat = native_function_new(spec, globals, defaults, kwdefaults, closure, doc);
    if (!nat) return NULL;
    if (!tramp_helper) {
        static const char *SRC =
            "import types, keyword\n"
            "_codes = {}\n"
            "def _ok(n):\n"
            "    return n.isidentifier() and not keyword.iskeyword(n) and not n.startswith('__fpy')\n"
            "def make(key, native, names, nposonly, npos, nkwonly, var, kw, defaults, kwdefaults, name, qualname, globs, doc, annots, asyn=False):\n"
            "    code = _codes.get((key, asyn))\n"
            "    if code is None:\n"
            "        nd = len(defaults) if defaults else 0\n"
            "        kwd = set(kwdefaults) if kwdefaults else set()\n"
            "        nm = [n if _ok(n) else '_fpy_p%d' % i for i, n in enumerate(names)]\n"
            "        ps, call = [], []\n"
            "        for i in range(npos):\n"
            "            ps.append(nm[i] + ('=None' if i >= npos - nd else ''))\n"
            "            call.append(nm[i])\n"
            "            if nposonly and i == nposonly - 1: ps.append('/')\n"
            "        j = npos + nkwonly\n"
            "        if var:\n"
            "            ps.append('*' + nm[j]); call.append('*' + nm[j]); j += 1\n"
            "        elif nkwonly:\n"
            "            ps.append('*')\n"
            "        for i in range(npos, npos + nkwonly):\n"
            "            ps.append(nm[i] + ('=None' if names[i] in kwd else '')); call.append('%s=%s' % (nm[i], nm[i]))\n"
            "        if kw:\n"
            "            ps.append('**' + nm[j]); call.append('**' + nm[j])\n"
            "        tpl = 'def __fpy_factory(__fpy_native__):\\n async def _fpy_fn(%s):\\n  return await __fpy_native__(%s)\\n return _fpy_fn\\n' if asyn else 'def __fpy_factory(__fpy_native__):\\n def _fpy_fn(%s):\\n  return __fpy_native__(%s)\\n return _fpy_fn\\n'\n"
            "        src = tpl % (', '.join(ps), ', '.join(call))\n"
            "        ns = {}\n"
            "        exec(src, {}, ns)\n"
            "        code = [c for c in ns['__fpy_factory'].__code__.co_consts if isinstance(c, types.CodeType)][0]\n"
            "        _codes[(key, asyn)] = code\n"
            "    f = types.FunctionType(code, globs, name, tuple(defaults) if defaults else None, (types.CellType(native),))\n"
            "    if kwdefaults: f.__kwdefaults__ = dict(kwdefaults)\n"
            "    f.__qualname__ = qualname\n"
            "    f.__doc__ = doc\n"
            "    if annots: f.__annotations__ = annots\n"
            "    f.__fpython_native__ = native\n"
            "    return f\n"
            "def make_async(*a):\n"
            "    return make(*a, asyn=True)\n";
        PyObject *g = PyDict_New();
        PyDict_SetItemString(g, "__builtins__", PyEval_GetBuiltins());
        PyObject *r = PyRun_String(SRC, Py_file_input, g, g);
        if (!r) { Py_DECREF(g); Py_DECREF(nat); return NULL; }
        Py_DECREF(r);
        tramp_helper = Py_NewRef(PyDict_GetItemString(g, "make"));
        tramp_async_helper = Py_NewRef(PyDict_GetItemString(g, "make_async"));
        Py_DECREF(g);
    }
    int n = spec->npos + spec->nkwonly + spec->has_varargs + spec->has_varkw;
    PyObject *names = PyTuple_New(n);
    for (int i = 0; i < n; i++) PyTuple_SET_ITEM(names, i, PyUnicode_FromString(spec->params[i]));
    PyObject *annots = fpc_annotations((FpcFunction *)nat);
    if (PyDict_GET_SIZE(annots) == 0) { Py_DECREF(annots); annots = Py_NewRef(Py_None); }
    PyObject *key = PyLong_FromVoidPtr((void *)spec);
    PyObject *f = PyObject_CallFunction(spec->is_coroutine ? tramp_async_helper : tramp_helper, "(NONiiiiiOOssOOO)", key, nat, names, spec->nposonly, spec->npos, spec->nkwonly,
                                        spec->has_varargs, spec->has_varkw, defaults ? defaults : Py_None, kwdefaults ? kwdefaults : Py_None,
                                        spec->name, spec->qualname, globals, doc ? doc : Py_None, annots);
    Py_DECREF(annots);
    Py_DECREF(nat);
    return f;
}

PyObject *fpc_closure_cell(PyObject *fn, int i) { return PyTuple_GET_ITEM(((FpcFunction *)fn)->closure, i); }
PyObject *fpc_globals(PyObject *fn) { return ((FpcFunction *)fn)->globals; }

/* ------------------------------------------------------------------ name lookups */
PyObject *fpc_load_global(PyObject *globals, PyObject *name) {
    PyObject *v = PyDict_GetItemWithError(globals, name);
    if (v) return Py_NewRef(v);
    if (PyErr_Occurred()) return NULL;
    PyObject *b = PyEval_GetBuiltins();
    v = PyDict_GetItemWithError(b, name);
    if (v) return Py_NewRef(v);
    if (!PyErr_Occurred()) PyErr_Format(PyExc_NameError, "name '%U' is not defined", name);
    return NULL;
}

/* Class bodies: namespace first, then globals/builtins. */
PyObject *fpc_load_name(PyObject *ns, PyObject *globals, PyObject *name) {
    PyObject *v = PyObject_GetItem(ns, name);
    if (v) return v;
    if (!PyErr_ExceptionMatches(PyExc_KeyError)) return NULL;
    PyErr_Clear();
    return fpc_load_global(globals, name);
}

PyObject *fpc_unbound_local(const char *name) {
    PyErr_Format(PyExc_UnboundLocalError, "cannot access local variable '%s' where it is not associated with a value", name);
    return NULL;
}

/* ------------------------------------------------------------------ helpers for generated code */
PyObject *fpc_import(PyObject *globals, const char *name, PyObject *fromlist, int level) {
    PyObject *n = PyUnicode_FromString(name);
    PyObject *r = PyImport_ImportModuleLevelObject(n, globals, NULL, fromlist, level);
    Py_DECREF(n);
    return r;
}

PyObject *fpc_import_from(PyObject *module, PyObject *name) {
    PyObject *v = PyObject_GetAttr(module, name);
    if (v) return v;
    if (!PyErr_ExceptionMatches(PyExc_AttributeError)) return NULL;
    PyErr_Clear();
    PyObject *pkg = PyObject_GetAttrString(module, "__name__");
    if (!pkg) return NULL;
    PyObject *full = PyUnicode_FromFormat("%U.%U", pkg, name);
    Py_DECREF(pkg);
    v = PyImport_GetModule(full);
    if (!v && !PyErr_Occurred()) {
        PyErr_Format(PyExc_ImportError, "cannot import name '%U'", name);
    }
    Py_DECREF(full);
    return v;
}

int fpc_import_star(PyObject *module, PyObject *ns) {
    PyObject *all = PyObject_GetAttrString(module, "__all__");
    PyObject *names;
    if (all) {
        names = PySequence_List(all);
        Py_DECREF(all);
    } else {
        PyErr_Clear();
        PyObject *d = PyObject_GetAttrString(module, "__dict__");
        if (!d) return -1;
        names = PyDict_Keys(d);
        Py_DECREF(d);
    }
    if (!names) return -1;
    for (Py_ssize_t i = 0; i < PyList_GET_SIZE(names); i++) {
        PyObject *n = PyList_GET_ITEM(names, i);
        if (!all && PyUnicode_READ_CHAR(n, 0) == '_') continue;
        PyObject *v = PyObject_GetAttr(module, n);
        if (!v || PyObject_SetItem(ns, n, v) < 0) { Py_XDECREF(v); Py_DECREF(names); return -1; }
        Py_DECREF(v);
    }
    Py_DECREF(names);
    return 0;
}

/* Iterable unpacking into exactly n (or at least n with a star at position `star`) targets. */
PyObject *fpc_unpack(PyObject *seq, Py_ssize_t before, int has_star, Py_ssize_t after) {
    PyObject *l = PySequence_List(seq);
    if (!l) return NULL;
    Py_ssize_t n = PyList_GET_SIZE(l);
    if (!has_star && n != before) {
        PyErr_Format(n > before ? PyExc_ValueError : PyExc_ValueError, n > before ? "too many values to unpack (expected %zd)" : "not enough values to unpack (expected %zd, got %zd)", before, n);
        Py_DECREF(l);
        return NULL;
    }
    if (has_star) {
        if (n < before + after) {
            PyErr_Format(PyExc_ValueError, "not enough values to unpack (expected at least %zd, got %zd)", before + after, n);
            Py_DECREF(l);
            return NULL;
        }
        /* repack as [before..., [middle], after...] */
        PyObject *mid = PyList_GetSlice(l, before, n - after);
        PyObject *r = PyList_New(before + 1 + after);
        for (Py_ssize_t i = 0; i < before; i++) PyList_SET_ITEM(r, i, Py_NewRef(PyList_GET_ITEM(l, i)));
        PyList_SET_ITEM(r, before, mid);
        for (Py_ssize_t i = 0; i < after; i++) PyList_SET_ITEM(r, before + 1 + i, Py_NewRef(PyList_GET_ITEM(l, n - after + i)));
        Py_DECREF(l);
        return r;
    }
    return l;
}

PyObject *fpc_call_star(PyObject *f, PyObject *args_list, PyObject *kwargs) {
    PyObject *t = PySequence_Tuple(args_list);
    if (!t) return NULL;
    PyObject *r = PyObject_Call(f, t, kwargs);
    Py_DECREF(t);
    return r;
}

PyObject *fpc_inplace_pow(PyObject *a, PyObject *b) { return PyNumber_InPlacePower(a, b, Py_None); }

int fpc_exc_matches(PyObject *exc, PyObject *cls) { return PyErr_GivenExceptionMatches(exc, cls); }

int fpc_raise(PyObject *exc, PyObject *cause) {
    if (!exc) {
        PyObject *cur = PyErr_GetHandledException();
        if (!cur || cur == Py_None) {
            Py_XDECREF(cur);
            PyErr_SetString(PyExc_RuntimeError, "No active exception to reraise");
            return -1;
        }
        PyErr_SetRaisedException(cur);
        return -1;
    }
    PyObject *inst = exc;
    if (PyExceptionClass_Check(exc)) inst = PyObject_CallNoArgs(exc);
    else Py_INCREF(inst);
    if (!inst) return -1;
    if (!PyExceptionInstance_Check(inst)) {
        Py_DECREF(inst);
        PyErr_SetString(PyExc_TypeError, "exceptions must derive from BaseException");
        return -1;
    }
    if (cause) {
        PyObject *c = cause == Py_None ? Py_NewRef(Py_None) : (PyExceptionClass_Check(cause) ? PyObject_CallNoArgs(cause) : Py_NewRef(cause));
        PyException_SetCause(inst, c);
    }
    /* PyErr_SetObject chains the handled exception as __context__, like the RAISE opcode */
    PyErr_SetObject((PyObject *)Py_TYPE(inst), inst);
    Py_DECREF(inst);
    return -1;
}

static PyObject *class_helpers;

static PyObject *class_helper(const char *name) {
    if (!class_helpers) {
        static const char *SRC =
            "import types\n"
            "def build(name, bases, ns, kwds):\n"
            "    resolved = types.resolve_bases(bases)\n"
            "    if resolved is not bases and resolved != bases: ns['__orig_bases__'] = bases\n"
            "    meta, _, kw = types.prepare_class(name, resolved, dict(kwds))\n"
            "    return meta(name, resolved, ns, **kw)\n"
            "def prepare(name, bases, kwds):\n"
            "    meta, ns, kw = types.prepare_class(name, types.resolve_bases(bases), dict(kwds))\n"
            "    return ns\n";
        class_helpers = PyDict_New();
        PyDict_SetItemString(class_helpers, "__builtins__", PyEval_GetBuiltins());
        PyObject *r = PyRun_String(SRC, Py_file_input, class_helpers, class_helpers);
        if (!r) return NULL;
        Py_DECREF(r);
    }
    return PyDict_GetItemString(class_helpers, name);
}

PyObject *fpc_build_class(PyObject *name, PyObject *bases, PyObject *ns, PyObject *kwds) {
    PyObject *h = class_helper("build");
    if (!h) return NULL;
    return PyObject_CallFunctionObjArgs(h, name, bases, ns, kwds, NULL);
}

/* __prepare__ support: namespace for a class body */
PyObject *fpc_prepare(PyObject *name, PyObject *bases, PyObject *kwds) {
    PyObject *h = class_helper("prepare");
    if (!h) return NULL;
    return PyObject_CallFunctionObjArgs(h, name, bases, kwds, NULL);
}

PyObject *fpc_format_value(PyObject *v, int conv, PyObject *spec) {
    PyObject *x;
    if (conv == 'r') x = PyObject_Repr(v);
    else if (conv == 's') x = PyObject_Str(v);
    else if (conv == 'a') x = PyObject_ASCII(v);
    else x = Py_NewRef(v);
    if (!x) return NULL;
    PyObject *r = PyObject_Format(x, spec);
    Py_DECREF(x);
    return r;
}

/* ------------------------------------------------------------------ compiled generators */

typedef struct {
    PyObject_HEAD
    FpyIter *gen;       /* coroutine running the compiled body (pinned in the fpython GC) */
    PyObject *fn;       /* the function object (closure, globals) */
    PyObject **slots;   /* bound arguments, owned by the body once started */
    int nslots;
    FpcGenBody body;
    PyObject *result;   /* return value once finished */
    PyObject *sent;     /* value passed by send() */
    PyObject *thrown;   /* exception to raise at the suspension point */
    int started, done, failed, running;
    const char *name;
} CompiledGen;

static PyObject *compiled_gen_type;

static void cg_trampoline(FpyGen *g) {
    CompiledGen *cg = (CompiledGen *)fpy_gen_frame(g);
    PyObject *r = cg->body(cg->fn, cg->slots, g);
    cg->slots = NULL; /* consumed by the body */
    if (!r) cg->failed = 1;
    else cg->result = r;
}

/* `yield v` inside a compiled generator: steals v; returns the sent value (new ref) or NULL. */
PyObject *fpc_yield(FpyGen *g, PyObject *v) {
    CompiledGen *cg = (CompiledGen *)fpy_gen_frame(g);
    fpy_gen_yield(g, (fpy_word)(uintptr_t)v);
    if (cg->thrown) {
        PyObject *e = cg->thrown;
        cg->thrown = NULL;
        PyErr_SetRaisedException(e);
        return NULL;
    }
    PyObject *s = cg->sent ? cg->sent : Py_NewRef(Py_None);
    cg->sent = NULL;
    return s;
}

/* `yield from it`: delegate send/throw to `it`; returns the sub-iterator's return value. */
PyObject *fpc_yield_from(FpyGen *g, PyObject *iterable) {
    PyObject *it = PyObject_GetIter(iterable);
    if (!it) return NULL;
    CompiledGen *cg = (CompiledGen *)fpy_gen_frame(g);
    PyObject *send = Py_NewRef(Py_None);
    for (;;) {
        PyObject *res = NULL;
        PySendResult r = PyIter_Send(it, send, &res);
        Py_DECREF(send);
        if (r == PYGEN_RETURN) { Py_DECREF(it); return res; }
        if (r == PYGEN_ERROR) { Py_DECREF(it); return NULL; }
        fpy_gen_yield(g, (fpy_word)(uintptr_t)res);
        if (cg->thrown) {
            PyObject *e = cg->thrown;
            cg->thrown = NULL;
            PyObject *t = PyObject_GetAttrString(it, "throw");
            if (!t) { PyErr_Clear(); Py_DECREF(it); PyErr_SetRaisedException(e); return NULL; }
            PyObject *y = PyObject_CallOneArg(t, e);
            Py_DECREF(t);
            Py_DECREF(e);
            if (!y) {
                if (PyErr_ExceptionMatches(PyExc_StopIteration)) {
                    PyObject *ex = PyErr_GetRaisedException();
                    PyObject *val = PyObject_GetAttrString(ex, "value");
                    Py_DECREF(ex);
                    Py_DECREF(it);
                    return val;
                }
                Py_DECREF(it);
                return NULL;
            }
            send = NULL;
            /* the inner iterator yielded again in response to throw */
            fpy_gen_yield(g, (fpy_word)(uintptr_t)y);
        }
        send = cg->sent ? cg->sent : Py_NewRef(Py_None);
        cg->sent = NULL;
    }
}

/* `await x` inside a compiled coroutine. */
PyObject *fpc_await(FpyGen *g, PyObject *aw) {
    PyTypeObject *tp = Py_TYPE(aw);
    PyObject *it = NULL;
    if (tp->tp_as_async && tp->tp_as_async->am_await) it = tp->tp_as_async->am_await(aw);
    else {
        PyObject *m = PyObject_GetAttrString(aw, "__await__");
        if (!m) {
            PyErr_Clear();
            PyErr_Format(PyExc_TypeError, "object %.100s can't be used in 'await' expression", tp->tp_name);
            return NULL;
        }
        it = PyObject_CallNoArgs(m);
        Py_DECREF(m);
    }
    if (!it) return NULL;
    PyObject *r = fpc_yield_from(g, it);
    Py_DECREF(it);
    return r;
}

/* Awaits and releases `aw` (which may be NULL on error). */
PyObject *fpc_await_steal(FpyGen *g, PyObject *aw) {
    if (!aw) return NULL;
    PyObject *r = fpc_await(g, aw);
    Py_DECREF(aw);
    return r;
}

/* Resume; returns a yielded value (new ref), or NULL when finished/failed (sets StopIteration
 * with the return value when `want_stop_value`, or leaves the error from the body). */
static PyObject *cg_step(CompiledGen *cg, PyObject *sent, PyObject *thrown, int want_stop_value) {
    if (cg->done) {
        Py_XDECREF(sent);
        if (thrown) { PyErr_SetRaisedException(thrown); return NULL; }
        return NULL;
    }
    if (cg->running) {
        Py_XDECREF(sent);
        Py_XDECREF(thrown);
        PyErr_SetString(PyExc_ValueError, "generator already executing");
        return NULL;
    }
    if (!cg->started && thrown) {
        /* throwing into a fresh generator: it finishes without running */
        cg->done = 1;
        Py_XDECREF(sent);
        PyErr_SetRaisedException(thrown);
        return NULL;
    }
    cg->started = 1;
    cg->sent = sent;
    cg->thrown = thrown;
    cg->running = 1;
    int dummy;
    thread_enter(&dummy);
    fpy_word out = 0;
    int y = 0;
    FpyExcFrame f;
    if (FPY_TRY(f)) {
        y = fpy_gen_step(cg->gen, 0, NULL, &out);
        FPY_POP(f);
    } else {
        FPY_POP(f);
        set_py_error_from(fpy_cur_exc);
        cg->failed = 1;
    }
    thread_leave();
    cg->running = 0;
    if (y) return (PyObject *)(uintptr_t)out;
    cg->done = 1;
    unpin((FpyObj *)cg->gen);
    if (cg->failed) {
        if (PyErr_ExceptionMatches(PyExc_StopIteration)) {
            /* PEP 479 */
            PyObject *e = PyErr_GetRaisedException();
            PyErr_SetString(PyExc_RuntimeError, "generator raised StopIteration");
            PyObject *now = PyErr_GetRaisedException();
            PyException_SetCause(now, e);
            PyErr_SetRaisedException(now);
        }
        return NULL;
    }
    if (want_stop_value && cg->result && cg->result != Py_None) {
        PyObject *exc = PyObject_CallOneArg(PyExc_StopIteration, cg->result);
        if (exc) { PyErr_SetObject(PyExc_StopIteration, exc); Py_DECREF(exc); }
    } else if (want_stop_value == 1) {
        PyErr_SetNone(PyExc_StopIteration);
    }
    return NULL;
}

/* Like CPython's gen_iternext: exhaustion is a bare NULL, except that a non-None return value is
 * reported as StopIteration(value), which `yield from`/`await` (the SEND opcode) read. */
static PyObject *cg_iternext(PyObject *self) { return cg_step((CompiledGen *)self, NULL, NULL, 2); }
static PyObject *cg_send(PyObject *self, PyObject *v) {
    CompiledGen *cg = (CompiledGen *)self;
    if (!cg->started && v != Py_None) {
        PyErr_SetString(PyExc_TypeError, "can't send non-None value to a just-started generator");
        return NULL;
    }
    return cg_step(cg, Py_NewRef(v), NULL, 1);
}
static PyObject *cg_throw(PyObject *self, PyObject *const *args, Py_ssize_t n) {
    if (n < 1) { PyErr_SetString(PyExc_TypeError, "throw expects an exception"); return NULL; }
    PyObject *e = args[n >= 2 && args[1] != Py_None ? 1 : 0];
    PyObject *inst = PyExceptionClass_Check(e) ? PyObject_CallNoArgs(e) : Py_NewRef(e);
    if (!inst) return NULL;
    return cg_step((CompiledGen *)self, NULL, inst, 1);
}
static PyObject *cg_close(PyObject *self, PyObject *unused) {
    (void)unused;
    CompiledGen *cg = (CompiledGen *)self;
    if (cg->done || !cg->started) { cg->done = 1; Py_RETURN_NONE; }
    PyObject *r = cg_step(cg, NULL, PyObject_CallNoArgs(PyExc_GeneratorExit), 0);
    if (r) {
        Py_DECREF(r);
        PyErr_SetString(PyExc_RuntimeError, "generator ignored GeneratorExit");
        return NULL;
    }
    if (PyErr_Occurred()) {
        if (PyErr_ExceptionMatches(PyExc_GeneratorExit) || PyErr_ExceptionMatches(PyExc_StopIteration)) PyErr_Clear();
        else return NULL;
    }
    Py_RETURN_NONE;
}

static void cg_dealloc(PyObject *self) {
    CompiledGen *cg = (CompiledGen *)self;
    if (cg->started && !cg->done) {
        PyObject *exc = PyErr_GetRaisedException();
        PyObject *r = cg_close(self, NULL);
        if (!r) PyErr_WriteUnraisable(self);
        Py_XDECREF(r);
        PyErr_SetRaisedException(exc);
    }
    if (!cg->done && cg->gen) unpin((FpyObj *)cg->gen);
    if (cg->slots) {
        for (int i = 0; i < cg->nslots; i++) Py_XDECREF(cg->slots[i]);
    }
    free(cg->slots);
    Py_XDECREF(cg->fn);
    Py_XDECREF(cg->result);
    Py_XDECREF(cg->sent);
    Py_XDECREF(cg->thrown);
    Py_TYPE(self)->tp_free(self);
}

static PyObject *cg_repr(PyObject *self) {
    return PyUnicode_FromFormat("<compiled generator %s at %p>", ((CompiledGen *)self)->name, self);
}

static PyObject *cg_iter(PyObject *self) { return Py_NewRef(self); }

static PyMethodDef cg_methods[] = {
    {"send", (PyCFunction)cg_send, METH_O, NULL},
    {"throw", (PyCFunction)(void (*)(void))cg_throw, METH_FASTCALL, NULL},
    {"close", (PyCFunction)cg_close, METH_NOARGS, NULL},
    {NULL},
};

static PyType_Slot cg_slots[] = {
    {Py_tp_iter, cg_iter},
    {Py_tp_iternext, cg_iternext},
    {Py_tp_methods, cg_methods},
    {Py_tp_dealloc, cg_dealloc},
    {Py_tp_repr, cg_repr},
    {0, NULL},
};

static PyType_Spec cg_spec = {.name = "fpython.CompiledGenerator", .basicsize = sizeof(CompiledGen), .flags = Py_TPFLAGS_DEFAULT, .slots = cg_slots};

static PySendResult cc_am_send(PyObject *self, PyObject *arg, PyObject **presult) {
    CompiledGen *cg = (CompiledGen *)self;
    PyObject *r = cg_step(cg, (arg && arg != Py_None) ? Py_NewRef(arg) : NULL, NULL, 0);
    if (r) { *presult = r; return PYGEN_NEXT; }
    if (PyErr_Occurred()) { *presult = NULL; return PYGEN_ERROR; }
    *presult = Py_NewRef(cg->result ? cg->result : Py_None);
    return PYGEN_RETURN;
}
static PyObject *cc_await(PyObject *self) { return Py_NewRef(self); }
static PyObject *cc_repr(PyObject *self) {
    return PyUnicode_FromFormat("<compiled coroutine %s at %p>", ((CompiledGen *)self)->name, self);
}
static PyType_Slot cc_slots[] = {
    {Py_am_await, cc_await},
    {Py_am_send, cc_am_send},
    {Py_tp_iter, cc_await},
    {Py_tp_iternext, cg_iternext},
    {Py_tp_methods, cg_methods},
    {Py_tp_dealloc, cg_dealloc},
    {Py_tp_repr, cc_repr},
    {0, NULL},
};
static PyType_Spec cc_spec = {.name = "fpython.CompiledCoroutine", .basicsize = sizeof(CompiledGen), .flags = Py_TPFLAGS_DEFAULT, .slots = cc_slots};
static PyObject *compiled_coro_type;

static PyObject *make_cg_type(PyType_Spec *spec, const char *abc_name) {
    PyObject *t = PyType_FromSpec(spec);
    if (!t) return NULL;
    /* register with collections.abc so isinstance checks (asyncio.iscoroutine, ...) pass */
    PyObject *abc = PyImport_ImportModule("collections.abc");
    if (abc) {
        PyObject *g = PyObject_GetAttrString(abc, abc_name);
        if (g) {
            PyObject *r = PyObject_CallMethod(g, "register", "O", t);
            Py_XDECREF(r);
            Py_DECREF(g);
        }
        Py_DECREF(abc);
    }
    PyErr_Clear();
    return t;
}

static PyObject *new_compiled_gen(PyObject *type, FpcGenBody body, PyObject *fn, PyObject **slots, int nslots, const char *name);

/* Called by a compiled generator function: takes ownership of the slot references. */
PyObject *fpc_make_generator(FpcGenBody body, PyObject *fn, PyObject **slots, int nslots, const char *name) {
    if (!compiled_gen_type && !(compiled_gen_type = make_cg_type(&cg_spec, "Generator"))) return NULL;
    return new_compiled_gen(compiled_gen_type, body, fn, slots, nslots, name);
}

/* Called by a compiled `async def`: returns a coroutine object. */
PyObject *fpc_make_coroutine(FpcGenBody body, PyObject *fn, PyObject **slots, int nslots, const char *name) {
    if (!compiled_coro_type && !(compiled_coro_type = make_cg_type(&cc_spec, "Coroutine"))) return NULL;
    return new_compiled_gen(compiled_coro_type, body, fn, slots, nslots, name);
}

static PyObject *new_compiled_gen(PyObject *type, FpcGenBody body, PyObject *fn, PyObject **slots, int nslots, const char *name) {
    CompiledGen *cg = PyObject_New(CompiledGen, (PyTypeObject *)type);
    if (!cg) return NULL;
    cg->body = body;
    cg->fn = Py_NewRef(fn);
    cg->nslots = nslots;
    cg->slots = malloc(sizeof(PyObject *) * (size_t)(nslots ? nslots : 1));
    for (int i = 0; i < nslots; i++) cg->slots[i] = slots[i];
    cg->result = cg->sent = cg->thrown = NULL;
    cg->started = cg->done = cg->failed = cg->running = 0;
    cg->name = name;
    cg->gen = fpy_gen_new(cg_trampoline, cg, FK_OBJ);
    pin((FpyObj *)cg->gen);
    return (PyObject *)cg;
}

/* ------------------------------------------------------------------ module registry + finder */
typedef int (*FpcExec)(PyObject *module);
typedef struct { const char *name; FpcExec exec; int is_package; } FpcModule;

static const FpcModule *fpc_modules;
static int fpc_nmodules;

static PyObject *fpc_exec_capsule(PyObject *self, PyObject *args) {
    (void)self;
    const char *name;
    PyObject *module;
    if (!PyArg_ParseTuple(args, "sO", &name, &module)) return NULL;
    for (int i = 0; i < fpc_nmodules; i++)
        if (strcmp(fpc_modules[i].name, name) == 0) {
            if (fpc_modules[i].exec(module) < 0) return NULL;
            Py_RETURN_NONE;
        }
    PyErr_Format(PyExc_ImportError, "no compiled module %s", name);
    return NULL;
}

static PyMethodDef fpc_exec_def = {"_fpy_exec_compiled", fpc_exec_capsule, METH_VARARGS, NULL};

static const char *FINDER =
    "import sys, importlib.abc, importlib.util, importlib.machinery\n"
    "class _FpyCompiledFinder(importlib.abc.MetaPathFinder):\n"
    "    def __init__(self, names, execf):\n"
    "        self.names = names; self.execf = execf\n"
    "    def find_spec(self, fullname, path=None, target=None):\n"
    "        if fullname not in self.names: return None\n"
    "        # locate the real source spec (for __file__, __path__, resources) but execute native code\n"
    "        for f in sys.meta_path:\n"
    "            if f is self: continue\n"
    "            spec = f.find_spec(fullname, path, target) if hasattr(f, 'find_spec') else None\n"
    "            if spec is not None: break\n"
    "        else:\n"
    "            spec = None\n"
    "        loader = _FpyCompiledLoader(self.execf, spec.loader if spec else None)\n"
    "        new = importlib.util.spec_from_loader(fullname, loader, origin=spec.origin if spec else None,\n"
    "                                              is_package=self.names[fullname])\n"
    "        if spec is not None and spec.submodule_search_locations is not None:\n"
    "            new.submodule_search_locations = spec.submodule_search_locations\n"
    "        new.has_location = spec is not None and spec.has_location\n"
    "        return new\n"
    "import types as _types\n"
    "def _fpy_mk(execf, name, module):\n"
    "    def _fpy_module_body():\n"
    "        return execf(name, module)\n"
    "    return _fpy_module_body\n"
    "_fpy_body_code = [c for c in _fpy_mk.__code__.co_consts if isinstance(c, _types.CodeType)][0]\n"
    "class _FpyCompiledLoader(importlib.abc.Loader):\n"
    "    def __init__(self, execf, fallback): self.execf = execf; self.fallback = fallback\n"
    "    def create_module(self, spec): return None\n"
    "    def exec_module(self, module):\n"
    "        try:\n"
    "            # run under a frame whose globals are the module namespace (sys._getframe users)\n"
    "            module.__dict__.setdefault('__builtins__', __builtins__)\n"
    "            cells = (_types.CellType(self.execf), _types.CellType(module), _types.CellType(module.__name__))\n"
    "            names = _fpy_body_code.co_freevars\n"
    "            cl = tuple({'execf': cells[0], 'module': cells[1], 'name': cells[2]}[n] for n in names)\n"
    "            _types.FunctionType(_fpy_body_code, module.__dict__, '<module>', None, cl)()\n"
    "            module.__fpython_compiled__ = True\n"
    "        except Exception:\n"
    "            if self.fallback is None or __import__('os').environ.get('FPY_NO_FALLBACK'): raise\n"
    "            spec = module.__spec__\n"
    "            if __import__('os').environ.get('FPY_VERBOSE'): print('fpython: native module', spec.name, 'failed at import; using CPython', file=sys.stderr)\n"
    "            module.__dict__.clear(); module.__dict__.update(__name__=spec.name, __spec__=spec, __loader__=self.fallback, __file__=spec.origin)\n"
    "            if spec.submodule_search_locations is not None: module.__path__ = list(spec.submodule_search_locations)\n"
    "            self.fallback.exec_module(module)\n"
    "    def get_resource_reader(self, name):\n"
    "        return self.fallback.get_resource_reader(name) if self.fallback and hasattr(self.fallback, 'get_resource_reader') else None\n"
    "def _fpy_install(names, execf):\n"
    "    sys.meta_path.insert(0, _FpyCompiledFinder(names, execf))\n";

int fpc_register_modules(const FpcModule *mods, int n) {
    fpc_modules = mods;
    fpc_nmodules = n;
    PyObject *g = PyDict_New();
    PyDict_SetItemString(g, "__builtins__", PyEval_GetBuiltins());
    PyObject *r = PyRun_String(FINDER, Py_file_input, g, g);
    if (!r) { Py_DECREF(g); return -1; }
    Py_DECREF(r);
    PyObject *names = PyDict_New();
    for (int i = 0; i < n; i++) {
        PyObject *b = PyBool_FromLong(mods[i].is_package);
        PyDict_SetItemString(names, mods[i].name, b);
        Py_DECREF(b);
    }
    PyObject *execf = PyCFunction_New(&fpc_exec_def, NULL);
    PyObject *inst = PyDict_GetItemString(g, "_fpy_install");
    PyObject *rr = PyObject_CallFunctionObjArgs(inst, names, execf, NULL);
    Py_DECREF(names);
    Py_DECREF(execf);
    Py_DECREF(g);
    if (!rr) return -1;
    Py_DECREF(rr);
    return 0;
}
#endif /* FPY_PYTHON */
