/* fpython runtime — CPython 3.14 bridge (compiled only with -DFPY_PYTHON).
 *
 * Python objects appear in fpython as `Any` values of type FT_PYOBJ (a GC'd proxy holding a strong
 * reference). Scalars, str, bytes, list, tuple, dict and set convert at the boundary; everything else
 * stays a proxy. Native functions handed to Python become `fpython.NativeFunction` objects carrying
 * a real `__signature__` so frameworks (FastAPI, fastmcp, ...) can introspect them.
 *
 * All fpython code runs while holding the GIL, which serializes it across threads. Threads that
 * enter fpython from Python register their stacks with the GC.
 */
#ifdef FPY_PYTHON
#define PY_SSIZE_T_CLEAN
#include <Python.h>
#include "fpy.h"

typedef struct FpyPyObj { const FpyType *ty; PyObject *o; } FpyPyObj;

static void pyobj_finalize(FpyObj *o) {
    FpyPyObj *p = (FpyPyObj *)o;
    if (p->o && Py_IsInitialized()) Py_DECREF(p->o);
    p->o = NULL;
}
static void pyobj_trace(FpyObj *o) { (void)o; }
static FpyStr *pyobj_repr(FpyObj *o);
static FpyStr *pyobj_str(FpyObj *o);
static bool pyobj_eq(FpyObj *a, FpyObj *b);
static int64_t pyobj_hash(FpyObj *o);
static int pyobj_lt(FpyObj *a, FpyObj *b);

const FpyType fpy_type_pyobj = {.name = "PyObject", .tag = FT_PYOBJ, .trace = pyobj_trace, .finalize = pyobj_finalize,
                                .repr = pyobj_repr, .str = pyobj_str, .eq = pyobj_eq, .hash = pyobj_hash, .lt = pyobj_lt};

static PyObject *native_function_type;
static PyObject *make_sig_helper;   /* Python helper building inspect.Signature */
static PyObject *make_class_helper; /* Python helper creating classes with native methods */
FpyObj *fpy_missing;

/* ------------------------------------------------------------------ pinning (Python -> fpy refs) */
static FpyObj **pins;
static int64_t npins, cappins;

static void pin(FpyObj *o) {
    if (npins == cappins) {
        cappins = cappins ? cappins * 2 : 64;
        pins = realloc(pins, (size_t)cappins * sizeof(FpyObj *));
    }
    pins[npins++] = o;
}
static void unpin(FpyObj *o) {
    for (int64_t i = 0; i < npins; i++)
        if (pins[i] == o) { pins[i] = pins[--npins]; return; }
}
void fpy_py_mark_roots(void) {
    for (int64_t i = 0; i < npins; i++) fpy_gc_mark(pins[i]);
    if (fpy_missing) fpy_gc_mark(fpy_missing);
}

/* ------------------------------------------------------------------ errors */
_Noreturn static void raise_py_error(void) {
    PyObject *exc = PyErr_GetRaisedException();
    if (!exc) fpy_raise_code(FPY_EXC_RUNTIME, "unknown Python error");
    FpyPyObj *w = fpy_alloc(&fpy_type_pyobj, sizeof(FpyPyObj));
    w->o = exc; /* steals */
    fpy_raise((FpyObj *)w);
}

static PyObject *check(PyObject *r) {
    if (!r) raise_py_error();
    return r;
}

static PyObject *builtin_exc(const char *name);

/* ------------------------------------------------------------------ conversions */
FpyObj *fpy_from_py(PyObject *o); /* new fpy value; does not steal */
PyObject *fpy_to_py(FpyObj *o);   /* new reference */

FpyObj *fpy_wrap_py(PyObject *o) { /* steals o */
    if (!o) raise_py_error();
    FpyPyObj *w = fpy_alloc(&fpy_type_pyobj, sizeof(FpyPyObj));
    w->o = o;
    return (FpyObj *)w;
}

FpyObj *fpy_from_py(PyObject *o) {
    if (!o || o == Py_None) return NULL;
    if (PyBool_Check(o)) return fpy_box_bool(o == Py_True);
    if (PyLong_CheckExact(o)) {
        int overflow = 0;
        long long v = PyLong_AsLongLongAndOverflow(o, &overflow);
        if (!overflow) return fpy_box_int(v);
        Py_INCREF(o);
        return fpy_wrap_py(o);
    }
    if (PyFloat_CheckExact(o)) return fpy_box_float(PyFloat_AS_DOUBLE(o));
    if (PyUnicode_CheckExact(o)) {
        Py_ssize_t n;
        const char *s = PyUnicode_AsUTF8AndSize(o, &n);
        if (!s) raise_py_error();
        return (FpyObj *)fpy_str_new(s, n);
    }
    if (PyBytes_CheckExact(o)) return (FpyObj *)fpy_bytes_new((const uint8_t *)PyBytes_AS_STRING(o), PyBytes_GET_SIZE(o));
    if (PyList_CheckExact(o)) {
        Py_ssize_t n = PyList_GET_SIZE(o);
        FpyList *l = fpy_list_new(FK_OBJ, n);
        for (Py_ssize_t i = 0; i < n; i++) fpy_list_append(l, fpy_wp(fpy_from_py(PyList_GET_ITEM(o, i))));
        return (FpyObj *)l;
    }
    if (PyTuple_CheckExact(o)) {
        Py_ssize_t n = PyTuple_GET_SIZE(o);
        FpyTuple *t = fpy_tuple_new(n, NULL);
        for (Py_ssize_t i = 0; i < n; i++) t->items[i] = fpy_wp(fpy_from_py(PyTuple_GET_ITEM(o, i)));
        return (FpyObj *)t;
    }
    if (PyDict_CheckExact(o)) {
        FpyDict *d = fpy_dict_new(FK_OBJ, FK_OBJ);
        PyObject *k, *v;
        Py_ssize_t pos = 0;
        while (PyDict_Next(o, &pos, &k, &v)) fpy_dict_set(d, fpy_wp(fpy_from_py(k)), fpy_wp(fpy_from_py(v)));
        return (FpyObj *)d;
    }
    if (PySet_CheckExact(o)) { /* frozensets stay Python objects (fpython has no frozenset type) */
        FpySet *s = fpy_set_new(FK_OBJ);
        PyObject *it = check(PyObject_GetIter(o)), *x;
        while ((x = PyIter_Next(it))) {
            fpy_set_add(s, fpy_wp(fpy_from_py(x)));
            Py_DECREF(x);
        }
        Py_DECREF(it);
        if (PyErr_Occurred()) raise_py_error();
        return (FpyObj *)s;
    }
    if (Py_IS_TYPE(o, (PyTypeObject *)native_function_type)) {
        /* round-trip of a native function: unwrap */
    }
    Py_INCREF(o);
    return fpy_wrap_py(o);
}

static PyObject *word_to_py(fpy_word w, int kind) {
    switch (kind) {
    case FK_INT: return PyLong_FromLongLong((long long)w);
    case FK_FLOAT: return PyFloat_FromDouble(fpy_fw(w));
    case FK_BOOL: return PyBool_FromLong(w != 0);
    default: return fpy_to_py(fpy_pw(w));
    }
}

PyObject *fpy_native_function(FpyFunc *f, const char *name, PyObject *sigspec);
static void pin(FpyObj *o);
static void unpin(FpyObj *o);
static void set_py_error_from(FpyObj *e);

/* Native iterators (ranges, generators, ...) seen from Python. */
typedef struct { PyObject_HEAD FpyIter *it; } NativeIter;
static PyObject *native_iter_type;

static PyObject *ni_next(PyObject *self) {
    NativeIter *ni = (NativeIter *)self;
    if (!ni->it) return NULL;
    PyObject *r = NULL;
    FpyExcFrame f;
    if (FPY_TRY(f)) {
        fpy_word w;
        if (ni->it->next(ni->it, &w)) r = word_to_py(w, ni->it->kind);
        else {
            unpin((FpyObj *)ni->it);
            ni->it = NULL;
        }
        FPY_POP(f);
    } else {
        FPY_POP(f);
        set_py_error_from(fpy_cur_exc);
    }
    return r;
}
static PyObject *ni_iter(PyObject *self) { return Py_NewRef(self); }
static void ni_dealloc(PyObject *self) {
    NativeIter *ni = (NativeIter *)self;
    if (ni->it) unpin((FpyObj *)ni->it);
    Py_TYPE(self)->tp_free(self);
}
static PyType_Slot ni_slots[] = {{Py_tp_iter, ni_iter}, {Py_tp_iternext, ni_next}, {Py_tp_dealloc, ni_dealloc}, {0, NULL}};
static PyType_Spec ni_spec = {.name = "fpython.NativeIterator", .basicsize = sizeof(NativeIter), .flags = Py_TPFLAGS_DEFAULT, .slots = ni_slots};

PyObject *fpy_to_py(FpyObj *o) {
    if (!o) Py_RETURN_NONE;
    switch (o->ty->tag) {
    case FT_PYOBJ: return Py_NewRef(((FpyPyObj *)o)->o);
    case FT_INT: return PyLong_FromLongLong(fpy_unbox_int(o));
    case FT_FLOAT: return PyFloat_FromDouble(fpy_unbox_float(o));
    case FT_BOOL: return PyBool_FromLong(((FpyBox *)o)->v != 0);
    case FT_STR: return PyUnicode_FromStringAndSize(((FpyStr *)o)->data, ((FpyStr *)o)->len);
    case FT_BYTES: return PyBytes_FromStringAndSize((const char *)((FpyBytes *)o)->data, ((FpyBytes *)o)->len);
    case FT_LIST: {
        FpyList *l = (FpyList *)o;
        PyObject *r = PyList_New(l->len);
        for (int64_t i = 0; i < l->len; i++) PyList_SET_ITEM(r, i, word_to_py(l->items[i], l->kind));
        return r;
    }
    case FT_TUPLE: {
        FpyTuple *t = (FpyTuple *)o;
        PyObject *r = PyTuple_New(t->len);
        for (int64_t i = 0; i < t->len; i++) PyTuple_SET_ITEM(r, i, word_to_py(t->items[i], t->kinds[i]));
        return r;
    }
    case FT_DICT: {
        FpyDict *d = (FpyDict *)o;
        PyObject *r = PyDict_New();
        FpyDictEntry *e;
        for (int64_t i = 0; fpy_dict_next(d, &i, &e);) {
            PyObject *k = word_to_py(e->key, d->kkind), *v = word_to_py(e->value, d->vkind);
            PyDict_SetItem(r, k, v);
            Py_DECREF(k);
            Py_DECREF(v);
        }
        return r;
    }
    case FT_SET: {
        FpyDict *d = (FpyDict *)o;
        PyObject *r = PySet_New(NULL);
        FpyDictEntry *e;
        for (int64_t i = 0; fpy_dict_next(d, &i, &e);) {
            PyObject *k = word_to_py(e->key, d->kkind);
            PySet_Add(r, k);
            Py_DECREF(k);
        }
        return r;
    }
    case FT_FUNC: return fpy_native_function((FpyFunc *)o, ((FpyFunc *)o)->name, NULL);
    case FT_ITER: {
        if (!native_iter_type) native_iter_type = PyType_FromSpec(&ni_spec);
        NativeIter *ni = PyObject_New(NativeIter, (PyTypeObject *)native_iter_type);
        ni->it = (FpyIter *)o;
        pin(o);
        return (PyObject *)ni;
    }
    case FT_CLASS: {
        /* native exception classes map to the nearest builtin Python exception class */
        for (const FpyType *t = ((FpyClassObj *)o)->cls; t; t = t->base) {
            PyObject *b = builtin_exc(t->name);
            if (b) return Py_NewRef(b);
        }
        break;
    }
    case FT_INSTANCE: {
        /* native exceptions become real Python exceptions */
        for (const FpyType *t = o->ty; t; t = t->base) {
            if (strcmp(t->name, "BaseException") != 0 && t->base) continue;
            PyObject *cls = NULL;
            for (const FpyType *x = o->ty; x && !cls; x = x->base) cls = builtin_exc(x->name);
            if (cls && strcmp(t->name, "BaseException") == 0) {
                FpyStr *msg = fpy_obj_str(o);
                return check(PyObject_CallFunction(cls, "s#", msg->data, (Py_ssize_t)msg->len));
            }
            break;
        }
        /* native objects cross as SimpleNamespace snapshots of their fields */
        PyObject *ns_mod = PyImport_ImportModule("types");
        PyObject *ns_cls = PyObject_GetAttrString(ns_mod, "SimpleNamespace");
        PyObject *kw = PyDict_New();
        for (const FpyType *t = o->ty; t; t = t->base)
            for (int32_t i = 0; i < t->nfields; i++) {
                const FpyField *f = &t->fields[i];
                char *p = (char *)o + f->offset;
                fpy_word w = f->kind == FK_BOOL ? (fpy_word) * (bool *)p : *(fpy_word *)p;
                PyObject *v = word_to_py(w, f->kind);
                PyDict_SetItemString(kw, f->name, v);
                Py_DECREF(v);
            }
        PyObject *args = PyTuple_New(0);
        PyObject *r = PyObject_Call(ns_cls, args, kw);
        Py_DECREF(args);
        Py_DECREF(kw);
        Py_DECREF(ns_cls);
        Py_DECREF(ns_mod);
        return check(r);
    }
    }
    FpyStr *s = fpy_obj_repr(o);
    return PyUnicode_FromStringAndSize(s->data, s->len);
}

/* ------------------------------------------------------------------ threads */
typedef struct ThreadRec {
    unsigned long id;
    void *bottom;
    void *saved_sp; /* while executing Python code */
    jmp_buf regs;
    int depth;
    int in_python;
} ThreadRec;

static ThreadRec threads[256];
static int nthreads;
static _Thread_local ThreadRec *cur_thread;

static ThreadRec *thread_enter(void *bottom) {
    if (cur_thread) { cur_thread->depth++; cur_thread->in_python = 0; return cur_thread; }
    for (int i = 0; i < nthreads; i++)
        if (threads[i].id == 0) { cur_thread = &threads[i]; break; }
    if (!cur_thread) {
        if (nthreads >= 256) { fprintf(stderr, "fpython: too many threads\n"); abort(); }
        cur_thread = &threads[nthreads++];
    }
    cur_thread->id = PyThread_get_thread_ident();
    cur_thread->bottom = bottom;
    cur_thread->depth = 1;
    cur_thread->in_python = 0;
    return cur_thread;
}

static void thread_leave(void) {
    if (--cur_thread->depth == 0) {
        cur_thread->id = 0;
        cur_thread = NULL;
    } else {
        cur_thread->in_python = 1;
    }
}

/* Called before fpython code calls into Python (which may release the GIL). */
#define PY_ENTER() ThreadRec *_tr = cur_thread; if (_tr) { setjmp(_tr->regs); _tr->saved_sp = __builtin_frame_address(0); _tr->in_python = 1; }
#define PY_LEAVE() if (_tr) _tr->in_python = 0;

extern void fpy_gc_scan_range_pub(void *lo, void *hi);

/* GC hook: scan stacks of other threads that are currently inside Python. */
void fpy_py_scan_threads(void) {
    for (int i = 0; i < nthreads; i++) {
        ThreadRec *t = &threads[i];
        if (!t->id || t == cur_thread || !t->in_python) continue;
        fpy_gc_scan_range_pub(t->saved_sp, t->bottom);
        fpy_gc_scan_range_pub(&t->regs, (char *)&t->regs + sizeof(t->regs));
    }
}

/* Is the current thread a foreign (non-main) fpython thread? Its own stack bottom for GC. */
void *fpy_py_thread_bottom(void) { return cur_thread && cur_thread != &threads[0] ? cur_thread->bottom : NULL; }

/* ------------------------------------------------------------------ dynamic operations */
static PyObject *P(FpyObj *o) { return ((FpyPyObj *)o)->o; }

static FpyObj *py_getattr(FpyObj *o, const char *name) {
    PY_ENTER();
    PyObject *r = PyObject_GetAttrString(P(o), name);
    PY_LEAVE();
    if (!r) raise_py_error();
    FpyObj *v = fpy_from_py(r);
    Py_DECREF(r);
    return v;
}

static void py_setattr(FpyObj *o, const char *name, FpyObj *v) {
    PyObject *pv = fpy_to_py(v);
    PY_ENTER();
    int rc = PyObject_SetAttrString(P(o), name, pv);
    PY_LEAVE();
    Py_DECREF(pv);
    if (rc < 0) raise_py_error();
}

static PyObject *build_args(FpyObj **args, int64_t n) {
    PyObject *t = PyTuple_New(n);
    for (int64_t i = 0; i < n; i++) PyTuple_SET_ITEM(t, i, fpy_to_py(args[i]));
    return t;
}

static PyObject *build_kwargs(const char **names, FpyObj **vals, int64_t nk) {
    if (!nk) return NULL;
    PyObject *d = PyDict_New();
    for (int64_t i = 0; i < nk; i++) {
        PyObject *v = fpy_to_py(vals[i]);
        PyDict_SetItemString(d, names[i], v);
        Py_DECREF(v);
    }
    return d;
}

static FpyObj *call_py(PyObject *callable, FpyObj **args, int64_t n, const char **kwn, FpyObj **kwv, int64_t nk) {
    PyObject *a = build_args(args, n);
    PyObject *k = build_kwargs(kwn, kwv, nk);
    PY_ENTER();
    PyObject *r = PyObject_Call(callable, a, k);
    PY_LEAVE();
    Py_DECREF(a);
    Py_XDECREF(k);
    if (!r) raise_py_error();
    FpyObj *v = fpy_from_py(r);
    Py_DECREF(r);
    return v;
}

static FpyObj *py_call(FpyObj *f, FpyObj **args, int64_t n, const char **kwn, FpyObj **kwv, int64_t nk) {
    return call_py(P(f), args, n, kwn, kwv, nk);
}

static FpyObj *py_call_method(FpyObj *o, const char *name, FpyObj **args, int64_t n, const char **kwn, FpyObj **kwv, int64_t nk) {
    PyObject *recv = (o && o->ty->tag == FT_PYOBJ) ? Py_NewRef(P(o)) : fpy_to_py(o);
    PyObject *m = PyObject_GetAttrString(recv, name);
    Py_DECREF(recv);
    if (!m) raise_py_error();
    FpyObj *r = call_py(m, args, n, kwn, kwv, nk);
    Py_DECREF(m);
    return r;
}

static FpyObj *py_getitem(FpyObj *o, FpyObj *k) {
    PyObject *pk = fpy_to_py(k);
    PY_ENTER();
    PyObject *r = PyObject_GetItem(P(o), pk);
    PY_LEAVE();
    Py_DECREF(pk);
    if (!r) raise_py_error();
    FpyObj *v = fpy_from_py(r);
    Py_DECREF(r);
    return v;
}

static void py_setitem(FpyObj *o, FpyObj *k, FpyObj *v) {
    PyObject *pk = fpy_to_py(k), *pv = fpy_to_py(v);
    int rc = PyObject_SetItem(P(o), pk, pv);
    Py_DECREF(pk);
    Py_DECREF(pv);
    if (rc < 0) raise_py_error();
}

static fpy_int py_len(FpyObj *o) {
    Py_ssize_t n = PyObject_Length(P(o));
    if (n < 0) raise_py_error();
    return n;
}

static bool py_truthy(FpyObj *o) {
    int r = PyObject_IsTrue(P(o));
    if (r < 0) raise_py_error();
    return r;
}

static bool py_contains(FpyObj *c, FpyObj *x) {
    PyObject *px = fpy_to_py(x);
    int r = PySequence_Contains(P(c), px);
    Py_DECREF(px);
    if (r < 0) raise_py_error();
    return r;
}

static bool py_iter_next(FpyIter *it, fpy_word *out) {
    PyObject *pit = P(it->o1);
    PY_ENTER();
    PyObject *x = PyIter_Next(pit);
    PY_LEAVE();
    if (!x) {
        if (PyErr_Occurred()) raise_py_error();
        return false;
    }
    *out = fpy_wp(fpy_from_py(x));
    Py_DECREF(x);
    return true;
}

static FpyIter *py_iter(FpyObj *o) {
    PyObject *pit = PyObject_GetIter(P(o));
    if (!pit) raise_py_error();
    FpyIter *it = fpy_iter_new(py_iter_next, FK_OBJ);
    it->o1 = fpy_wrap_py(pit);
    return it;
}

static FpyObj *py_binop(int op, FpyObj *a, FpyObj *b) {
    PyObject *pa = fpy_to_py(a), *pb = fpy_to_py(b), *r = NULL;
    switch (op) {
    case 0: r = PyNumber_Add(pa, pb); break;
    case 1: r = PyNumber_Subtract(pa, pb); break;
    case 2: r = PyNumber_Multiply(pa, pb); break;
    case 3: r = PyNumber_TrueDivide(pa, pb); break;
    case 4: r = PyNumber_FloorDivide(pa, pb); break;
    case 5: r = PyNumber_Remainder(pa, pb); break;
    case 6: r = PyNumber_Power(pa, pb, Py_None); break;
    case 7: r = PyNumber_Lshift(pa, pb); break;
    case 8: r = PyNumber_Rshift(pa, pb); break;
    case 9: r = PyNumber_And(pa, pb); break;
    case 10: r = PyNumber_Or(pa, pb); break;
    case 11: r = PyNumber_Xor(pa, pb); break;
    }
    Py_DECREF(pa);
    Py_DECREF(pb);
    if (!r) raise_py_error();
    FpyObj *v = fpy_from_py(r);
    Py_DECREF(r);
    return v;
}

static int py_compare(int op, FpyObj *a, FpyObj *b) {
    static const int ops[] = {Py_EQ, Py_NE, Py_LT, Py_LE, Py_GT, Py_GE};
    PyObject *pa = fpy_to_py(a), *pb = fpy_to_py(b);
    int r = PyObject_RichCompareBool(pa, pb, ops[op]);
    Py_DECREF(pa);
    Py_DECREF(pb);
    if (r < 0) raise_py_error();
    return r;
}

static FpyStr *pystr(PyObject *s) {
    if (!s) raise_py_error();
    Py_ssize_t n;
    const char *d = PyUnicode_AsUTF8AndSize(s, &n);
    FpyStr *r = fpy_str_new(d, n);
    Py_DECREF(s);
    return r;
}
static FpyStr *pyobj_repr(FpyObj *o) { return pystr(PyObject_Repr(P(o))); }
static FpyStr *pyobj_str(FpyObj *o) { return pystr(PyObject_Str(P(o))); }
static bool pyobj_eq(FpyObj *a, FpyObj *b) { return py_compare(0, a, b); }
static int64_t pyobj_hash(FpyObj *o) {
    Py_hash_t h = PyObject_Hash(P(o));
    if (h == -1) raise_py_error();
    return h;
}
static int pyobj_lt(FpyObj *a, FpyObj *b) { return py_compare(2, a, b); }

/* ------------------------------------------------------------------ exceptions */
static PyObject *builtin_exc(const char *name) {
    PyObject *b = PyEval_GetFrameBuiltins();
    PyObject *r = b ? PyDict_GetItemString(b, name) : NULL;
    if (!r) {
        PyObject *bm = PyImport_ImportModule("builtins");
        r = bm ? PyObject_GetAttrString(bm, name) : NULL;
        Py_XDECREF(bm);
        if (r) Py_DECREF(r); /* borrowed semantics: builtins module keeps it alive */
    }
    PyErr_Clear();
    return r && PyExceptionClass_Check(r) ? r : NULL;
}

/* Does exception `e` match native class `t`? (handles Python exceptions vs builtin classes) */
static bool py_exc_matches_native(FpyObj *e, const FpyType *t) {
    if (!e || e->ty->tag != FT_PYOBJ) return false;
    for (const FpyType *x = t; x; x = x->base) {
        (void)x;
        break;
    }
    PyObject *cls = builtin_exc(t->name);
    if (!cls) return false;
    return PyErr_GivenExceptionMatches(P(e), cls);
}

static bool py_exc_matches_dyn(FpyObj *e, FpyObj *cls) {
    if (!cls) return false;
    if (cls->ty->tag != FT_PYOBJ) return false;
    PyObject *pc = P(cls);
    if (e && e->ty->tag == FT_PYOBJ) return PyErr_GivenExceptionMatches(P(e), pc);
    /* native exception vs Python class: match by builtin name */
    if (e) {
        for (const FpyType *t = e->ty; t; t = t->base) {
            PyObject *b = builtin_exc(t->name);
            if (b && PyObject_IsSubclass(b, pc) == 1) return true;
        }
    }
    return false;
}

/* Raise the current fpython exception into Python (for native code called from Python). */
static void set_py_error_from(FpyObj *e) {
    if (e && e->ty->tag == FT_PYOBJ) {
        PyObject *o = P(e);
        if (PyExceptionInstance_Check(o)) PyErr_SetRaisedException(Py_NewRef(o));
        else PyErr_SetObject(PyExc_RuntimeError, o);
        return;
    }
    PyObject *cls = NULL;
    for (const FpyType *t = e ? e->ty : NULL; t && !cls; t = t->base) cls = builtin_exc(t->name);
    if (!cls) cls = PyExc_RuntimeError;
    FpyStr *msg = fpy_obj_str(e);
    PyErr_SetString(cls, msg->data);
}

/* ------------------------------------------------------------------ NativeFunction */
typedef struct {
    PyObject_HEAD
    FpyFunc *f;
    PyObject *dict;
    PyObject *param_names; /* tuple of str or NULL */
    PyObject *bound_self;  /* unused; binding handled via descriptor */
} NativeFunction;

static PyObject *nf_call(PyObject *self, PyObject *args, PyObject *kwargs) {
    NativeFunction *nf = (NativeFunction *)self;
    int dummy;
    thread_enter(&dummy);
    Py_ssize_t n = PyTuple_GET_SIZE(args);
    Py_ssize_t np = nf->param_names ? PyTuple_GET_SIZE(nf->param_names) : n;
    if (np < n) np = n;
    FpyObj *argv[64];
    if (np > 64) {
        thread_leave();
        PyErr_SetString(PyExc_TypeError, "too many arguments for native function");
        return NULL;
    }
    int64_t count = n;
    FpyExcFrame f;
    PyObject *result = NULL;
    if (FPY_TRY(f)) {
        for (Py_ssize_t i = 0; i < n; i++) argv[i] = fpy_from_py(PyTuple_GET_ITEM(args, i));
        for (Py_ssize_t i = n; i < np; i++) argv[i] = fpy_missing;
        if (kwargs && PyDict_GET_SIZE(kwargs)) {
            if (!nf->param_names) fpy_raise_code(FPY_EXC_TYPE, "native function takes no keyword arguments");
            PyObject *k, *v;
            Py_ssize_t pos = 0;
            while (PyDict_Next(kwargs, &pos, &k, &v)) {
                Py_ssize_t idx = -1;
                for (Py_ssize_t j = 0; j < PyTuple_GET_SIZE(nf->param_names); j++)
                    if (PyUnicode_Compare(k, PyTuple_GET_ITEM(nf->param_names, j)) == 0) { idx = j; break; }
                if (idx < 0) fpy_raise_fmt(FPY_EXC_TYPE, "%s() got an unexpected keyword argument '%s'", nf->f->name, PyUnicode_AsUTF8(k));
                argv[idx] = fpy_from_py(v);
                if (idx + 1 > count) count = idx + 1;
            }
        }
        FpyObj *r;
        if (!nf->f->dyn) fpy_raise_code(FPY_EXC_TYPE, "this native function cannot be called from Python (no dynamic entry)");
        if (nf->f->self) {
            FpyObj *argv2[65];
            argv2[0] = nf->f->self;
            for (int64_t i = 0; i < count; i++) argv2[i + 1] = argv[i];
            r = nf->f->dyn(nf->f->env, argv2, count + 1);
        } else {
            r = nf->f->dyn(nf->f->env, argv, count);
        }
        result = fpy_to_py(r);
        FPY_POP(f);
    } else {
        FPY_POP(f);
        set_py_error_from(fpy_cur_exc);
        result = NULL;
    }
    thread_leave();
    return result;
}

static void nf_dealloc(PyObject *self) {
    NativeFunction *nf = (NativeFunction *)self;
    PyObject_GC_UnTrack(self);
    if (nf->f) unpin((FpyObj *)nf->f);
    Py_CLEAR(nf->dict);
    Py_CLEAR(nf->param_names);
    Py_TYPE(self)->tp_free(self);
}

static int nf_traverse(PyObject *self, visitproc visit, void *arg) {
    Py_VISIT(((NativeFunction *)self)->dict);
    Py_VISIT(Py_TYPE(self));
    return 0;
}

static PyObject *nf_descr_get(PyObject *self, PyObject *obj, PyObject *type) {
    (void)type;
    if (!obj || obj == Py_None) return Py_NewRef(self);
    return PyMethod_New(self, obj);
}

static PyObject *nf_repr(PyObject *self) {
    return PyUnicode_FromFormat("<native function %s>", ((NativeFunction *)self)->f->name ? ((NativeFunction *)self)->f->name : "?");
}

static PyMemberDef nf_members[];
static PyType_Slot nf_slots[] = {
    {Py_tp_members, NULL},
    {Py_tp_call, nf_call},
    {Py_tp_dealloc, nf_dealloc},
    {Py_tp_traverse, nf_traverse},
    {Py_tp_descr_get, nf_descr_get},
    {Py_tp_repr, nf_repr},
    {0, NULL},
};

static PyMemberDef nf_members[] = {
    {"__dictoffset__", Py_T_PYSSIZET, offsetof(NativeFunction, dict), Py_READONLY},
    {NULL},
};

static PyType_Spec nf_spec = {
    .name = "fpython.NativeFunction",
    .basicsize = sizeof(NativeFunction),
    .flags = Py_TPFLAGS_DEFAULT | Py_TPFLAGS_HAVE_GC | Py_TPFLAGS_METHOD_DESCRIPTOR,
    .slots = nf_slots,
};

/* sigspec: (name, qualname, module, [(param, annotation_src, default_src|None), ...], return_src|None) */
PyObject *fpy_native_function(FpyFunc *f, const char *name, PyObject *sigspec) {
    NativeFunction *nf = PyObject_GC_New(NativeFunction, (PyTypeObject *)native_function_type);
    nf->f = f;
    nf->dict = PyDict_New();
    nf->param_names = NULL;
    nf->bound_self = NULL;
    pin((FpyObj *)f);
    PyObject_GC_Track((PyObject *)nf);
    PyObject *nm = PyUnicode_FromString(name ? name : "native");
    PyDict_SetItemString(nf->dict, "__name__", nm);
    PyDict_SetItemString(nf->dict, "__qualname__", nm);
    Py_DECREF(nm);
    if (sigspec) {
        PyObject *r = PyObject_CallOneArg(make_sig_helper, sigspec);
        if (!r) {
            PyErr_Print();
        } else {
            /* r = (signature, annotations, param_names) */
            PyDict_SetItemString(nf->dict, "__signature__", PyTuple_GET_ITEM(r, 0));
            PyDict_SetItemString(nf->dict, "__annotations__", PyTuple_GET_ITEM(r, 1));
            nf->param_names = Py_NewRef(PyTuple_GET_ITEM(r, 2));
            PyDict_SetItemString(nf->dict, "__module__", PyTuple_GET_ITEM(sigspec, 2));
            PyDict_SetItemString(nf->dict, "__qualname__", PyTuple_GET_ITEM(sigspec, 1));
            Py_DECREF(r);
        }
    }
    return (PyObject *)nf;
}

static PyObject *wrap_helper;

/* Generated code: wrap a native function into a real Python function with the native signature. */
FpyObj *fpy_py_native(FpyFunc *f, const char *name, const char *qualname, const char *module, int nparams,
                      const char **pnames, const char **pannots, const char **pdefaults, const char *ret,
                      int is_async, const char *doc) {
    PyObject *params = PyList_New(nparams);
    for (int i = 0; i < nparams; i++) {
        PyObject *d = pdefaults[i] ? PyUnicode_FromString(pdefaults[i]) : Py_NewRef(Py_None);
        PyList_SET_ITEM(params, i, Py_BuildValue("(ssN)", pnames[i], pannots[i] ? pannots[i] : "", d));
    }
    PyObject *spec = Py_BuildValue("(sssNz)", name, qualname, module, params, ret);
    PyObject *nf = fpy_native_function(f, name, spec);
    PyObject *w = PyObject_CallFunction(wrap_helper, "(NOiz)", nf, spec, is_async, doc);
    Py_DECREF(spec);
    if (!w) raise_py_error();
    return fpy_wrap_py(w);
}

/* Mark a native function wrapper as a coroutine function (for asyncio/FastAPI introspection). */
void fpy_py_mark_async(FpyObj *fn) {
    PyObject *insp = PyImport_ImportModule("inspect");
    if (!insp) raise_py_error();
    PyObject *r = PyObject_CallMethod(insp, "markcoroutinefunction", "O", P(fn));
    Py_DECREF(insp);
    if (!r) raise_py_error();
    Py_DECREF(r);
}

/* Create a Python class `name(bases...)` whose methods are native functions.
 * annots: list of (name, annotation source); defaults: list of (name, value source). */
FpyObj *fpy_py_make_class(const char *name, const char *module, FpyObj *bases, FpyObj *methods, FpyObj *annots, FpyObj *defaults) {
    PyObject *pb = fpy_to_py(bases);
    PyObject *pm = PyDict_New();
    FpyDict *d = (FpyDict *)methods;
    FpyDictEntry *e;
    for (int64_t i = 0; fpy_dict_next(d, &i, &e);) {
        PyObject *k = fpy_to_py(fpy_pw(e->key));
        PyObject *v = fpy_to_py(fpy_pw(e->value));
        PyDict_SetItem(pm, k, v);
        Py_DECREF(k);
        Py_DECREF(v);
    }
    PyObject *pa = fpy_to_py(annots), *pd = fpy_to_py(defaults);
    PyObject *r = PyObject_CallFunction(make_class_helper, "(sNNsNN)", name, pb, pm, module, pa, pd);
    if (!r) raise_py_error();
    return fpy_wrap_py(r);
}

/* ------------------------------------------------------------------ native coroutines (async def) */
typedef struct {
    PyObject_HEAD
    FpyIter *gen;
    const char *name;
} NativeCoro;

static PyObject *native_coro_type;

static PySendResult coro_send_impl(PyObject *self, PyObject *arg, PyObject **presult, FpyObj *thrown) {
    NativeCoro *c = (NativeCoro *)self;
    int dummy;
    thread_enter(&dummy);
    PySendResult rc = PYGEN_ERROR;
    FpyExcFrame f;
    if (FPY_TRY(f)) {
        fpy_word out;
        FpyObj *sent = (arg && arg != Py_None) ? fpy_from_py(arg) : NULL;
        int y = fpy_gen_step(c->gen, fpy_wp(sent), thrown, &out);
        if (y) {
            *presult = fpy_to_py(fpy_pw(out));
            rc = PYGEN_NEXT;
        } else {
            *presult = fpy_to_py(fpy_pw(fpy_gen_result(c->gen)));
            if (c->gen) unpin((FpyObj *)c->gen);
            rc = PYGEN_RETURN;
        }
        FPY_POP(f);
    } else {
        FPY_POP(f);
        set_py_error_from(fpy_cur_exc);
        *presult = NULL;
        rc = PYGEN_ERROR;
    }
    thread_leave();
    return rc;
}

static PySendResult coro_am_send(PyObject *self, PyObject *arg, PyObject **presult) {
    return coro_send_impl(self, arg, presult, NULL);
}

static PyObject *finish_send(PySendResult rc, PyObject *r) {
    if (rc == PYGEN_NEXT) return r;
    if (rc == PYGEN_RETURN) {
        PyObject *exc = PyObject_CallOneArg(PyExc_StopIteration, r);
        Py_DECREF(r);
        if (exc) {
            PyErr_SetObject(PyExc_StopIteration, exc);
            Py_DECREF(exc);
        }
        return NULL;
    }
    return NULL;
}

static PyObject *coro_send(PyObject *self, PyObject *arg) {
    PyObject *r = NULL;
    PySendResult rc = coro_send_impl(self, arg, &r, NULL);
    return finish_send(rc, r);
}

static PyObject *coro_iternext(PyObject *self) {
    PyObject *r = NULL;
    PySendResult rc = coro_send_impl(self, Py_None, &r, NULL);
    if (rc == PYGEN_RETURN) {
        PyObject *exc = PyObject_CallOneArg(PyExc_StopIteration, r);
        Py_DECREF(r);
        if (exc) { PyErr_SetObject(PyExc_StopIteration, exc); Py_DECREF(exc); }
        return NULL;
    }
    return rc == PYGEN_NEXT ? r : NULL;
}

static PyObject *coro_throw(PyObject *self, PyObject *const *args, Py_ssize_t n) {
    if (n < 1) { PyErr_SetString(PyExc_TypeError, "throw expects an exception"); return NULL; }
    PyObject *e = args[n >= 2 && args[1] != Py_None ? 1 : 0];
    PyObject *inst = PyExceptionClass_Check(e) ? PyObject_CallNoArgs(e) : Py_NewRef(e);
    if (!inst) return NULL;
    FpyObj *thrown = fpy_wrap_py(inst);
    PyObject *r = NULL;
    PySendResult rc = coro_send_impl(self, Py_None, &r, thrown);
    return finish_send(rc, r);
}

static PyObject *coro_close(PyObject *self, PyObject *unused) {
    (void)unused;
    NativeCoro *c = (NativeCoro *)self;
    if (c->gen && !fpy_gen_done(c->gen)) {
        unpin((FpyObj *)c->gen);
        c->gen = NULL;
    }
    Py_RETURN_NONE;
}

static PyObject *coro_await(PyObject *self) { return Py_NewRef(self); }

static void coro_dealloc(PyObject *self) {
    NativeCoro *c = (NativeCoro *)self;
    if (c->gen) unpin((FpyObj *)c->gen);
    Py_TYPE(self)->tp_free(self);
}

static PyObject *coro_repr(PyObject *self) {
    return PyUnicode_FromFormat("<native coroutine %s>", ((NativeCoro *)self)->name ? ((NativeCoro *)self)->name : "?");
}

static PyMethodDef coro_methods[] = {
    {"send", (PyCFunction)coro_send, METH_O, NULL},
    {"throw", (PyCFunction)(void (*)(void))coro_throw, METH_FASTCALL, NULL},
    {"close", (PyCFunction)coro_close, METH_NOARGS, NULL},
    {NULL},
};

static PyType_Slot coro_slots[] = {
    {Py_am_await, coro_await},
    {Py_am_send, coro_am_send},
    {Py_tp_iter, coro_await},
    {Py_tp_iternext, coro_iternext},
    {Py_tp_methods, coro_methods},
    {Py_tp_dealloc, coro_dealloc},
    {Py_tp_repr, coro_repr},
    {0, NULL},
};

static PyType_Spec coro_spec = {
    .name = "fpython.NativeCoroutine",
    .basicsize = sizeof(NativeCoro),
    .flags = Py_TPFLAGS_DEFAULT,
    .slots = coro_slots,
};

/* Generated code for `async def`: wrap the native generator as a Python awaitable (Any). */
FpyObj *fpy_coro_new(FpyIter *gen, const char *name) {
    NativeCoro *c = PyObject_New(NativeCoro, (PyTypeObject *)native_coro_type);
    c->gen = gen;
    c->name = name;
    pin((FpyObj *)gen);
    return fpy_wrap_py((PyObject *)c);
}

/* `await aw` inside a native coroutine: drive aw's iterator, yielding to the event loop. */
FpyObj *fpy_await(FpyGen *g, FpyObj *aw) {
    PyObject *pa = fpy_to_py(aw);
    PyTypeObject *tp = Py_TYPE(pa);
    PyObject *it = NULL;
    if (tp->tp_as_async && tp->tp_as_async->am_await) it = tp->tp_as_async->am_await(pa);
    else {
        PyObject *m = PyObject_GetAttrString(pa, "__await__");
        if (m) { it = PyObject_CallNoArgs(m); Py_DECREF(m); }
    }
    Py_DECREF(pa);
    if (!it) raise_py_error();
    PyObject *send_val = Py_NewRef(Py_None);
    for (;;) {
        PyObject *res = NULL;
        PY_ENTER();
        PySendResult r = PyIter_Send(it, send_val, &res);
        PY_LEAVE();
        Py_DECREF(send_val);
        if (r == PYGEN_RETURN) {
            Py_DECREF(it);
            FpyObj *v = fpy_from_py(res);
            Py_DECREF(res);
            return v;
        }
        if (r == PYGEN_ERROR) {
            Py_DECREF(it);
            raise_py_error();
        }
        /* suspend: hand the yielded object to whoever is driving us */
        FpyObj *yielded = fpy_wrap_py(res);
        fpy_gen_yield(g, fpy_wp(yielded));
        FpyObj *thrown = fpy_gen_take_thrown(g);
        if (thrown) {
            PyObject *pt = fpy_to_py(thrown);
            PyObject *tr = PyObject_CallMethod(it, "throw", "O", pt);
            Py_DECREF(pt);
            if (!tr) {
                PyObject *exc = PyErr_GetRaisedException();
                if (PyErr_GivenExceptionMatches(exc, PyExc_StopIteration)) {
                    PyObject *val = PyObject_GetAttrString(exc, "value");
                    Py_DECREF(exc);
                    Py_DECREF(it);
                    FpyObj *v = fpy_from_py(val);
                    Py_XDECREF(val);
                    return v;
                }
                Py_DECREF(it);
                PyErr_SetRaisedException(exc);
                raise_py_error();
            }
            /* the inner awaitable handled it and yielded again */
            yielded = fpy_wrap_py(tr);
            fpy_gen_yield(g, fpy_wp(yielded));
        }
        fpy_word sent = fpy_gen_take_sent(g);
        send_val = fpy_to_py(fpy_pw(sent));
    }
}

/* ------------------------------------------------------------------ module-level API for generated code */
FpyObj *fpy_py_import(const char *name, bool top_level) {
    PyObject *m = top_level ? PyImport_ImportModuleLevel(name, NULL, NULL, NULL, 0) : PyImport_ImportModule(name);
    if (!m) raise_py_error();
    return fpy_wrap_py(m);
}

FpyObj *fpy_py_from_import(const char *module, const char *attr) {
    PyObject *m = PyImport_ImportModule(module);
    if (!m) raise_py_error();
    PyObject *r = PyObject_GetAttrString(m, attr);
    if (!r) {
        PyErr_Clear();
        char full[512];
        snprintf(full, sizeof full, "%s.%s", module, attr);
        r = PyImport_ImportModule(full);
    }
    Py_DECREF(m);
    if (!r) raise_py_error();
    FpyObj *v = fpy_from_py(r);
    Py_DECREF(r);
    return v;
}

/* Publish a value as an attribute of the Python-visible __main__ module (for unittest discovery etc.) */
void fpy_py_publish(const char *name, FpyObj *v) {
    PyObject *m = PyImport_AddModule("__main__");
    PyObject *pv = fpy_to_py(v);
    PyObject_SetAttrString(m, name, pv);
    Py_DECREF(pv);
}

static const char *BOOTSTRAP =
    "import inspect, typing, types, sys\n"
    "_ns = {k: getattr(typing, k) for k in dir(typing) if not k.startswith('_')}\n"
    "_ns.update({'int': int, 'float': float, 'str': str, 'bool': bool, 'bytes': bytes, 'list': list, 'dict': dict,\n"
    "            'set': set, 'tuple': tuple, 'None': None, 'object': object})\n"
    "def _fpy_ann(src):\n"
    "    if not src: return inspect.Parameter.empty\n"
    "    try: return eval(src, dict(_ns), dict(vars(sys.modules['__main__'])))\n"
    "    except Exception: return typing.Any\n"
    "def _fpy_make_sig(spec):\n"
    "    name, qualname, module, params, ret = spec\n"
    "    ps = []; anns = {}; names = []\n"
    "    for (p, ann, default) in params:\n"
    "        if p.startswith('*'):\n"
    "            ps.append(inspect.Parameter(p[1:], inspect.Parameter.VAR_POSITIONAL)); continue\n"
    "        a = _fpy_ann(ann)\n"
    "        d = inspect.Parameter.empty\n"
    "        if default is not None:\n"
    "            try: d = eval(default, dict(_ns), dict(vars(sys.modules['__main__'])))\n"
    "            except Exception: d = inspect.Parameter.empty\n"
    "        ps.append(inspect.Parameter(p, inspect.Parameter.POSITIONAL_OR_KEYWORD, default=d, annotation=a))\n"
    "        if a is not inspect.Parameter.empty: anns[p] = a\n"
    "        names.append(p)\n"
    "    r = _fpy_ann(ret) if ret else inspect.Signature.empty\n"
    "    if ret: anns['return'] = r\n"
    "    return (inspect.Signature(ps, return_annotation=r), anns, tuple(names))\n"
    "def _fpy_wrap(native, spec, is_async, doc):\n"
    "    sig, anns, names = _fpy_make_sig(spec)\n"
    "    if is_async:\n"
    "        async def f(*args, **kwargs): return await native(*args, **kwargs)\n"
    "    else:\n"
    "        def f(*args, **kwargs): return native(*args, **kwargs)\n"
    "    f.__signature__ = sig; f.__annotations__ = anns\n"
    "    f.__name__ = spec[0]; f.__qualname__ = spec[1]; f.__module__ = spec[2]; f.__doc__ = doc\n"
    "    f.__fpython_native__ = native\n"
    "    return f\n"
    "def _fpy_make_class(name, bases, methods, module, annots, defaults):\n"
    "    g = dict(_ns); g.update(vars(sys.modules['__main__']))\n"
    "    ns = dict(methods); ns['__module__'] = module\n"
    "    if annots: ns['__annotations__'] = {n: _fpy_ann(src) for (n, src) in annots}\n"
    "    for (n, src) in defaults: ns[n] = eval(src, g)\n"
    "    return types.new_class(name, tuple(bases), {}, lambda d: d.update(ns))\n";


static const char *py_type_name(FpyObj *o) { return Py_TYPE(P(o))->tp_name; }

static FpyObj *py_exc_instance(FpyObj *o) {
    PyObject *x = P(o);
    if (PyExceptionClass_Check(x)) {
        PyObject *inst = PyObject_CallNoArgs(x);
        if (!inst) raise_py_error();
        return fpy_wrap_py(inst);
    }
    return o;
}

static void py_print_exc(FpyObj *o) {
    PyObject *x = P(o);
    if (PyExceptionInstance_Check(x)) {
        PyErr_DisplayException(x);
    } else {
        PyObject *r = PyObject_Repr(x);
        fprintf(stderr, "uncaught: %s\n", r ? PyUnicode_AsUTF8(r) : "?");
        Py_XDECREF(r);
    }
    fflush(stderr);
}

static int py_exit_code(FpyObj *o, bool *is_exit) {
    PyObject *x = P(o);
    *is_exit = PyErr_GivenExceptionMatches(x, PyExc_SystemExit);
    if (!*is_exit) return 1;
    PyObject *code = PyObject_GetAttrString(x, "code");
    int rc = 0;
    if (!code || code == Py_None) rc = 0;
    else if (PyLong_Check(code)) rc = (int)PyLong_AsLong(code);
    else {
        PyObject *s = PyObject_Str(code);
        if (s) fprintf(stderr, "%s\n", PyUnicode_AsUTF8(s));
        Py_XDECREF(s);
        rc = 1;
    }
    Py_XDECREF(code);
    PyErr_Clear();
    return rc;
}

static bool py_isinstance(FpyObj *o, FpyObj *cls) {
    PyObject *po = fpy_to_py(o);
    int r = PyObject_IsInstance(po, P(cls));
    Py_DECREF(po);
    if (r < 0) raise_py_error();
    return r;
}

/* super(cls, self) for methods of Python-derived classes */
FpyObj *fpy_py_super(FpyObj *cls, FpyObj *self) {
    PyObject *b = PyImport_ImportModule("builtins");
    PyObject *sup = PyObject_GetAttrString(b, "super");
    PyObject *ps = fpy_to_py(self);
    PyObject *r = PyObject_CallFunctionObjArgs(sup, P(cls), ps, NULL);
    Py_DECREF(ps);
    Py_DECREF(sup);
    Py_DECREF(b);
    if (!r) raise_py_error();
    return fpy_wrap_py(r);
}

/* Apply a Python builtin wrapper (staticmethod, classmethod, property) to a value. */
FpyObj *fpy_py_builtin_wrap(const char *builtin, FpyObj *v) {
    PyObject *b = PyImport_ImportModule("builtins");
    PyObject *w = PyObject_GetAttrString(b, builtin);
    PyObject *pv = fpy_to_py(v);
    PyObject *r = PyObject_CallOneArg(w, pv);
    Py_DECREF(pv);
    Py_DECREF(w);
    Py_DECREF(b);
    if (!r) raise_py_error();
    return fpy_wrap_py(r);
}
static struct FpyPyHooks hooks = {py_getattr, py_setattr, py_call, py_call_method, py_getitem, py_setitem, py_len, py_truthy,
                                  py_contains, py_iter, py_binop, py_compare, py_exc_matches_native, py_exc_matches_dyn,
                                  py_type_name, fpy_py_mark_roots, fpy_py_scan_threads, fpy_py_thread_bottom,
                                  py_exc_instance, py_print_exc, py_exit_code, py_isinstance};

#ifndef FPY_PYTHONHOME
#define FPY_PYTHONHOME NULL
#endif

void fpy_py_init(int argc, char **argv) {
    PyConfig config;
    PyConfig_InitPythonConfig(&config);
    config.install_signal_handlers = 1;
    const char *home = getenv("FPY_PYTHONHOME");
    if (!home) home = FPY_PYTHONHOME;
    if (home) PyConfig_SetBytesString(&config, &config.home, home);
    config.parse_argv = 0;
    config.buffered_stdio = 0;
    setvbuf(stdout, NULL, _IOLBF, 1 << 16);
    PyConfig_SetBytesArgv(&config, argc, argv);
    PyStatus st = Py_InitializeFromConfig(&config);
    PyConfig_Clear(&config);
    if (PyStatus_Exception(st)) Py_ExitStatusException(st);
    /* program directory (or the source directory under `fpython run`) on sys.path */
    if (argc > 0) {
        char dir[4096];
        const char *src_dir = getenv("FPY_SOURCE_DIR");
        snprintf(dir, sizeof dir, "%s/x", src_dir && *src_dir ? src_dir : argv[0]);
        if (!src_dir || !*src_dir) snprintf(dir, sizeof dir, "%s", argv[0]);
        char *slash = strrchr(dir, '/');
        if (slash) *slash = 0; else strcpy(dir, ".");
        PyObject *path = PySys_GetObject("path");
        PyObject *d = PyUnicode_FromString(dir);
        if (path) PyList_Insert(path, 0, d);
        Py_DECREF(d);
    }
    native_coro_type = PyType_FromSpec(&coro_spec);
    if (!native_coro_type) { PyErr_Print(); abort(); }
    nf_slots[0].pfunc = nf_members;
    native_function_type = PyType_FromSpec(&nf_spec);
    if (!native_function_type) { PyErr_Print(); abort(); }
    PyObject *g = PyDict_New();
    PyDict_SetItemString(g, "__builtins__", PyEval_GetBuiltins());
    PyObject *r = PyRun_String(BOOTSTRAP, Py_file_input, g, g);
    if (!r) { PyErr_Print(); abort(); }
    Py_DECREF(r);
    make_sig_helper = Py_NewRef(PyDict_GetItemString(g, "_fpy_make_sig"));
    make_class_helper = Py_NewRef(PyDict_GetItemString(g, "_fpy_make_class"));
    wrap_helper = Py_NewRef(PyDict_GetItemString(g, "_fpy_wrap"));
    Py_DECREF(g);
    fpy_missing = fpy_alloc(&fpy_type_raw, sizeof(FpyObj) * 2);
    thread_enter(__builtin_frame_address(0)); /* main thread record (index 0) */
    fpy_py = &hooks;
}

void fpy_py_finalize(void) {
    if (Py_IsInitialized()) {
        fflush(stdout);
        Py_FinalizeEx();
    }
}
#endif /* FPY_PYTHON */
