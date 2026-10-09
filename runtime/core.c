/* fpython runtime — exceptions, boxing, iterators, generic protocols, I/O, dynamic ops. */
#include "fpy.h"
#include <stdarg.h>
#include <time.h>
#include <errno.h>
#include <unistd.h>
#include <sys/stat.h>
#include <dirent.h>
#include <signal.h>

_Thread_local FpyExcFrame *fpy_exc_top;
_Thread_local FpyObj *fpy_cur_exc;
struct FpyPyHooks *fpy_py;
#ifndef FPY_PYTHON
FpyObj *fpy_missing;
#endif
FpyList *fpy_argv;
extern void fpy_str_init_cache(void);
extern void fpy_str_mark_cache(void);
extern void fpy_gen_mark_roots(void);

/* ================================================================== boxes */
static void box_trace(FpyObj *o) { (void)o; }
const FpyType fpy_type_int = {.name = "int", .tag = FT_INT, .trace = box_trace};
const FpyType fpy_type_float = {.name = "float", .tag = FT_FLOAT, .trace = box_trace};
const FpyType fpy_type_bool = {.name = "bool", .tag = FT_BOOL, .trace = box_trace};
const FpyType fpy_type_str = {.name = "str", .tag = FT_STR, .trace = box_trace};
const FpyType fpy_type_bytes = {.name = "bytes", .tag = FT_BYTES, .trace = box_trace};

static FpyBox *small_ints[262];
static FpyBox bool_true = {&fpy_type_bool, 1}, bool_false = {&fpy_type_bool, 0};

FpyObj *fpy_box_int(fpy_int v) {
    if (v >= -5 && v <= 256 && small_ints[v + 5]) return (FpyObj *)small_ints[v + 5];
    FpyBox *b = fpy_alloc(&fpy_type_int, sizeof(FpyBox));
    b->v = (fpy_word)v;
    return (FpyObj *)b;
}
FpyObj *fpy_box_float(double v) {
    FpyBox *b = fpy_alloc(&fpy_type_float, sizeof(FpyBox));
    b->v = fpy_wf(v);
    return (FpyObj *)b;
}
FpyObj *fpy_box_bool(bool v) { return (FpyObj *)(v ? &bool_true : &bool_false); }

FpyObj *fpy_box_word(fpy_word w, int kind) {
    switch (kind) {
    case FK_INT: return fpy_box_int((fpy_int)w);
    case FK_FLOAT: return fpy_box_float(fpy_fw(w));
    case FK_BOOL: return fpy_box_bool(w != 0);
    default: return (FpyObj *)fpy_pw(w);
    }
}

fpy_word fpy_unbox_word(FpyObj *o, int kind, const char *tyname) {
    switch (kind) {
    case FK_INT:
        if (o && (o->ty == &fpy_type_int || o->ty == &fpy_type_bool)) return ((FpyBox *)o)->v;
        break;
    case FK_FLOAT:
        if (o && o->ty == &fpy_type_float) return ((FpyBox *)o)->v;
        if (o && (o->ty == &fpy_type_int || o->ty == &fpy_type_bool)) return fpy_wf((double)(fpy_int)((FpyBox *)o)->v);
        break;
    case FK_BOOL:
        if (o && o->ty == &fpy_type_bool) return ((FpyBox *)o)->v;
        break;
    default: return fpy_wp(o);
    }
    fpy_raise_fmt(FPY_EXC_TYPE, "expected %s, got %s", tyname, fpy_type_name(o));
}

static void cell_trace(FpyObj *o) {
    FpyCell *c = (FpyCell *)o;
    if (c->kind == FK_OBJ && c->v) fpy_gc_mark(fpy_pw(c->v));
}
const FpyType fpy_type_cell = {.name = "cell", .tag = FT_CELL, .trace = cell_trace};
FpyCell *fpy_cell_new(int kind) {
    FpyCell *c = fpy_alloc(&fpy_type_cell, sizeof(FpyCell));
    c->kind = kind;
    return c;
}

static void func_trace(FpyObj *o) {
    FpyFunc *f = (FpyFunc *)o;
    if (f->env) fpy_gc_mark(f->env);
    if (f->self) fpy_gc_mark(f->self);
}
static FpyStr *func_repr(FpyObj *o) {
    FpyFunc *f = (FpyFunc *)o;
    char buf[256];
    int n = snprintf(buf, sizeof buf, "<%s %s>", f->self ? "bound method" : "function", f->name ? f->name : "<lambda>");
    return fpy_str_new(buf, n);
}
const FpyType fpy_type_func = {.name = "function", .tag = FT_FUNC, .trace = func_trace, .repr = func_repr};
FpyFunc *fpy_func_new(void *fn, void *env, FpyDynFn dyn, const char *name) {
    FpyFunc *f = fpy_alloc(&fpy_type_func, sizeof(FpyFunc));
    f->fn = fn;
    f->env = env;
    f->dyn = dyn;
    f->name = name;
    return f;
}

static void class_trace(FpyObj *o) { (void)o; }
static FpyStr *class_repr(FpyObj *o) {
    char buf[256];
    int n = snprintf(buf, sizeof buf, "<class '%s'>", ((FpyClassObj *)o)->cls->name);
    return fpy_str_new(buf, n);
}
const FpyType fpy_type_class = {.name = "type", .tag = FT_CLASS, .trace = class_trace, .repr = class_repr};
FpyObj *fpy_class_obj(const FpyType *cls) {
    FpyClassObj *c = fpy_alloc(&fpy_type_class, sizeof(FpyClassObj));
    c->cls = cls;
    return (FpyObj *)c;
}

/* ================================================================== exceptions */
static void print_uncaught(FpyObj *exc) {
    fflush(stdout);
    if (fpy_is_py(exc)) {
        fpy_py->print_exc(exc);
        return;
    }
    FpyStr *msg = fpy_obj_str(exc);
    const char *name = exc ? fpy_type_name(exc) : "Exception";
    fprintf(stderr, "Traceback (most recent call last):\n  <compiled fpython code>\n");
    if (msg->len) fprintf(stderr, "%s: %s\n", name, msg->data);
    else fprintf(stderr, "%s\n", name);
}

_Noreturn void fpy_raise(FpyObj *exc) {
    fpy_cur_exc = exc;
    if (!fpy_exc_top) {
        print_uncaught(exc);
        exit(1);
    }
    longjmp(fpy_exc_top->jb, 1);
}

_Noreturn void fpy_reraise(void) { fpy_raise(fpy_cur_exc); }

_Noreturn void fpy_raise_code(int code, const char *msg) {
    fpy_raise(fpy_make_builtin_exc(code, fpy_str_from_cstr(msg)));
}

_Noreturn void fpy_raise_fmt(int code, const char *fmt, ...) {
    char buf[1024];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(buf, sizeof buf, fmt, ap);
    va_end(ap);
    fpy_raise_code(code, buf);
}

_Noreturn void fpy_overflow(void) { fpy_raise_code(FPY_EXC_OVERFLOW, "integer overflow (fpython ints are 64-bit)"); }
_Noreturn void fpy_zerodiv(void) { fpy_raise_code(FPY_EXC_ZERODIV, "division by zero"); }
_Noreturn void fpy_raise_stop_iteration(void) { fpy_raise_code(FPY_EXC_STOPITER, ""); }

fpy_int fpy_ipow(fpy_int a, fpy_int b) {
    if (b < 0) fpy_raise_code(FPY_EXC_VALUE, "negative exponent for int ** int (use float)");
    fpy_int r = 1;
    while (b) {
        if (b & 1) r = fpy_mul(r, a);
        b >>= 1;
        if (b) a = fpy_mul(a, a);
    }
    return r;
}

/* ================================================================== iterators */
static void iter_trace(FpyObj *o) {
    FpyIter *it = (FpyIter *)o;
    if (it->o1) fpy_gc_mark(it->o1);
    if (it->o2) fpy_gc_mark(it->o2);
    if (it->o3) fpy_gc_mark(it->o3);
    if (it->a) fpy_gc_mark(fpy_pw(it->a));
    if (it->b) fpy_gc_mark(fpy_pw(it->b));
    if (it->c) fpy_gc_mark(fpy_pw(it->c));
}
const FpyType fpy_type_iter = {.name = "iterator", .tag = FT_ITER, .trace = iter_trace};

FpyIter *fpy_iter_new(bool (*next)(FpyIter *, fpy_word *), int kind) {
    FpyIter *it = fpy_alloc(&fpy_type_iter, sizeof(FpyIter));
    it->next = next;
    it->kind = kind;
    return it;
}

static bool range_next(FpyIter *it, fpy_word *out) {
    fpy_int step = (fpy_int)it->c;
    if (step > 0 ? it->i >= it->n : it->i <= it->n) return false;
    *out = (fpy_word)it->i;
    it->i += step;
    return true;
}
FpyIter *fpy_range_iter(fpy_int start, fpy_int stop, fpy_int step) {
    if (step == 0) fpy_raise_code(FPY_EXC_VALUE, "range() arg 3 must not be zero");
    FpyIter *it = fpy_iter_new(range_next, FK_INT);
    it->i = start;
    it->n = stop;
    it->c = (fpy_word)step;
    return it;
}

static bool enum_next(FpyIter *it, fpy_word *out) {
    FpyIter *in = (FpyIter *)it->o1;
    fpy_word w;
    if (!in->next(in, &w)) return false;
    uint8_t kinds[2] = {FK_INT, (uint8_t)in->kind};
    FpyTuple *t = fpy_tuple_new(2, kinds);
    t->items[0] = (fpy_word)it->i++;
    t->items[1] = w;
    *out = fpy_wp(t);
    return true;
}
FpyIter *fpy_enumerate(FpyIter *in, fpy_int start) {
    FpyIter *it = fpy_iter_new(enum_next, FK_OBJ);
    it->o1 = (FpyObj *)in;
    it->i = start;
    return it;
}

static bool zip_next(FpyIter *it, fpy_word *out) {
    FpyIter *a = (FpyIter *)it->o1, *b = (FpyIter *)it->o2;
    fpy_word x, y;
    if (!a->next(a, &x) || !b->next(b, &y)) return false;
    uint8_t kinds[2] = {(uint8_t)a->kind, (uint8_t)b->kind};
    FpyTuple *t = fpy_tuple_new(2, kinds);
    t->items[0] = x;
    t->items[1] = y;
    *out = fpy_wp(t);
    return true;
}
/* Iterator over an object implementing __next__ (raising StopIteration at the end). */
static bool protocol_next(FpyIter *it, fpy_word *out) {
    if (it->n) return false; /* exhausted */
    FpyExcFrame f;
    FpyObj *volatile r = NULL;
    if (FPY_TRY(f)) {
        r = fpy_dyn_call_method(it->o1, "__next__", NULL, 0);
        FPY_POP(f);
    } else {
        FPY_POP(f);
        if (!fpy_exc_match(fpy_cur_exc, fpy_builtin_exc_type(FPY_EXC_STOPITER))) fpy_reraise();
        it->n = 1;
        return false;
    }
    *out = fpy_unbox_word(r, it->kind, "iterator element");
    return true;
}

FpyIter *fpy_iter_from_protocol(FpyObj *o, int kind) {
    FpyIter *it = fpy_iter_new(protocol_next, kind);
    it->o1 = o;
    return it;
}

FpyIter *fpy_zip(FpyIter *a, FpyIter *b) {
    FpyIter *it = fpy_iter_new(zip_next, FK_OBJ);
    it->o1 = (FpyObj *)a;
    it->o2 = (FpyObj *)b;
    return it;
}

/* ================================================================== generic protocols */
const char *fpy_type_name(FpyObj *o) {
    if (!o) return "NoneType";
    if (fpy_is_py(o)) return fpy_py->type_name(o);
    return o->ty && o->ty->name ? o->ty->name : "object";
}

FpyStr *fpy_default_repr(FpyObj *o) {
    char buf[256];
    int n = snprintf(buf, sizeof buf, "<%s object at %p>", fpy_type_name(o), (void *)o);
    return fpy_str_new(buf, n);
}

static FpyStr *seq_repr(const char *open, const char *close, fpy_word *items, int64_t n, const uint8_t *kinds, int kind,
                        bool tuple1) {
    FpySB sb;
    fpy_sb_init(&sb);
    fpy_sb_add_cstr(&sb, open);
    for (int64_t i = 0; i < n; i++) {
        if (i) fpy_sb_add(&sb, ", ", 2);
        fpy_sb_add_str(&sb, fpy_repr_word(items[i], kinds ? kinds[i] : kind));
    }
    if (tuple1 && n == 1) fpy_sb_add(&sb, ",", 1);
    fpy_sb_add_cstr(&sb, close);
    return fpy_sb_finish(&sb);
}

static int repr_depth;

FpyStr *fpy_obj_repr(FpyObj *o) {
    if (!o) return fpy_str_lit("None", 4, 4);
    if (++repr_depth > 200) { repr_depth = 0; fpy_raise_code(FPY_EXC_RECURSION, "maximum recursion depth exceeded in repr"); }
    FpyStr *r;
    const FpyType *t = o->ty;
    if (t->repr) { r = t->repr(o); repr_depth--; return r; }
    switch (t->tag) {
    case FT_STR: r = fpy_str_repr((FpyStr *)o); break;
    case FT_BYTES: r = fpy_bytes_repr((FpyBytes *)o); break;
    case FT_INT: r = fpy_int_to_str((fpy_int)((FpyBox *)o)->v); break;
    case FT_FLOAT: r = fpy_float_to_str(fpy_fw(((FpyBox *)o)->v)); break;
    case FT_BOOL: r = fpy_bool_to_str(((FpyBox *)o)->v != 0); break;
    case FT_LIST: {
        FpyList *l = (FpyList *)o;
        r = seq_repr("[", "]", l->items, l->len, NULL, l->kind, false);
        break;
    }
    case FT_TUPLE: {
        FpyTuple *tp = (FpyTuple *)o;
        r = seq_repr("(", ")", tp->items, tp->len, tp->kinds, 0, true);
        break;
    }
    case FT_DICT:
    case FT_SET: {
        FpyDict *d = (FpyDict *)o;
        if (d->is_set && d->len == 0) { r = fpy_str_lit("set()", 5, 5); break; }
        FpySB sb;
        fpy_sb_init(&sb);
        fpy_sb_add(&sb, "{", 1);
        FpyDictEntry *e;
        bool first = true;
        for (int64_t i = 0; fpy_dict_next(d, &i, &e);) {
            if (!first) fpy_sb_add(&sb, ", ", 2);
            first = false;
            fpy_sb_add_str(&sb, fpy_repr_word(e->key, d->kkind));
            if (!d->is_set) {
                fpy_sb_add(&sb, ": ", 2);
                fpy_sb_add_str(&sb, fpy_repr_word(e->value, d->vkind));
            }
        }
        fpy_sb_add(&sb, "}", 1);
        r = fpy_sb_finish(&sb);
        break;
    }
    case FT_RANGE: {
        FpyRange *rg = (FpyRange *)o;
        char buf[96];
        int n = rg->step == 1 ? snprintf(buf, sizeof buf, "range(%lld, %lld)", (long long)rg->start, (long long)rg->stop)
                              : snprintf(buf, sizeof buf, "range(%lld, %lld, %lld)", (long long)rg->start, (long long)rg->stop, (long long)rg->step);
        r = fpy_str_new(buf, n);
        break;
    }
    default: r = fpy_default_repr(o);
    }
    repr_depth--;
    return r;
}

FpyStr *fpy_obj_str(FpyObj *o) {
    if (!o) return fpy_str_lit("None", 4, 4);
    if (o->ty == &fpy_type_str) return (FpyStr *)o;
    if (o->ty->str) return o->ty->str(o);
    return fpy_obj_repr(o);
}

FpyStr *fpy_repr_word(fpy_word w, int kind) {
    switch (kind) {
    case FK_INT: return fpy_int_to_str((fpy_int)w);
    case FK_FLOAT: return fpy_float_to_str(fpy_fw(w));
    case FK_BOOL: return fpy_bool_to_str(w != 0);
    default: return fpy_obj_repr(fpy_pw(w));
    }
}

FpyStr *fpy_str_word(fpy_word w, int kind) {
    if (kind == FK_OBJ) return fpy_obj_str(fpy_pw(w));
    return fpy_repr_word(w, kind);
}

static bool is_num(FpyObj *o) { return o && (o->ty == &fpy_type_int || o->ty == &fpy_type_float || o->ty == &fpy_type_bool); }
static double num_val(FpyObj *o) { return o->ty == &fpy_type_float ? fpy_fw(((FpyBox *)o)->v) : (double)(fpy_int)((FpyBox *)o)->v; }

bool fpy_obj_eq(FpyObj *a, FpyObj *b) {
    if (a == b) return true;
    if (!a || !b) return false;
    if (is_num(a) && is_num(b)) {
        if (a->ty != &fpy_type_float && b->ty != &fpy_type_float) return ((FpyBox *)a)->v == ((FpyBox *)b)->v;
        return num_val(a) == num_val(b);
    }
    if (a->ty->eq) return a->ty->eq(a, b);
    if (a->ty->tag != b->ty->tag) return false;
    switch (a->ty->tag) {
    case FT_STR: return fpy_str_eq((FpyStr *)a, (FpyStr *)b);
    case FT_BYTES: return fpy_bytes_eq((FpyBytes *)a, (FpyBytes *)b);
    case FT_LIST: {
        FpyList *x = (FpyList *)a, *y = (FpyList *)b;
        if (x->len != y->len) return false;
        for (int64_t i = 0; i < x->len; i++) {
            if (x->kind == y->kind) { if (!fpy_eq_word(x->items[i], y->items[i], x->kind)) return false; }
            else if (!fpy_obj_eq(fpy_box_word(x->items[i], x->kind), fpy_box_word(y->items[i], y->kind))) return false;
        }
        return true;
    }
    case FT_TUPLE: return fpy_tuple_eq((FpyTuple *)a, (FpyTuple *)b);
    case FT_DICT:
    case FT_SET: return fpy_dict_eq((FpyDict *)a, (FpyDict *)b);
    case FT_CLASS: return ((FpyClassObj *)a)->cls == ((FpyClassObj *)b)->cls;
    default: return false;
    }
}

bool fpy_eq_word(fpy_word a, fpy_word b, int kind) {
    switch (kind) {
    case FK_INT:
    case FK_BOOL: return a == b;
    case FK_FLOAT: return fpy_fw(a) == fpy_fw(b);
    default: return fpy_obj_eq(fpy_pw(a), fpy_pw(b));
    }
}

static int64_t mix(uint64_t x) {
    x ^= x >> 33;
    x *= 0xff51afd7ed558ccdull;
    x ^= x >> 33;
    return (int64_t)x;
}

int64_t fpy_obj_hash(FpyObj *o) {
    if (!o) return 0x1234567;
    if (o->ty->hash) return o->ty->hash(o);
    switch (o->ty->tag) {
    case FT_STR: return fpy_str_hash((FpyStr *)o);
    case FT_INT:
    case FT_BOOL: return mix(((FpyBox *)o)->v);
    case FT_FLOAT: {
        double d = fpy_fw(((FpyBox *)o)->v);
        if (d == (double)(fpy_int)d) return mix((uint64_t)(fpy_int)d);
        return mix(((FpyBox *)o)->v);
    }
    case FT_BYTES: {
        FpyBytes *b = (FpyBytes *)o;
        uint64_t h = 1469598103934665603ull;
        for (int64_t i = 0; i < b->len; i++) { h ^= b->data[i]; h *= 1099511628211ull; }
        return (int64_t)h;
    }
    case FT_TUPLE: {
        FpyTuple *t = (FpyTuple *)o;
        uint64_t h = 0x345678;
        for (int64_t i = 0; i < t->len; i++) h = (h ^ (uint64_t)fpy_hash_word(t->items[i], t->kinds[i])) * 1000003ull;
        return (int64_t)h;
    }
    case FT_LIST:
    case FT_DICT:
    case FT_SET: fpy_raise_fmt(FPY_EXC_TYPE, "unhashable type: '%s'", o->ty->name);
    default: return mix((uint64_t)(uintptr_t)o);
    }
}

int64_t fpy_hash_word(fpy_word w, int kind) {
    switch (kind) {
    case FK_INT:
    case FK_BOOL: return mix(w);
    case FK_FLOAT: {
        double d = fpy_fw(w);
        if (d == (double)(fpy_int)d) return mix((uint64_t)(fpy_int)d);
        return mix(w);
    }
    default: return fpy_obj_hash(fpy_pw(w));
    }
}

int fpy_obj_cmp(FpyObj *a, FpyObj *b) {
    if (is_num(a) && is_num(b)) {
        if (a->ty != &fpy_type_float && b->ty != &fpy_type_float) {
            fpy_int x = (fpy_int)((FpyBox *)a)->v, y = (fpy_int)((FpyBox *)b)->v;
            return x < y ? -1 : (x > y);
        }
        double x = num_val(a), y = num_val(b);
        return x < y ? -1 : (x > y);
    }
    if (a && b && a->ty->lt) {
        int r = a->ty->lt(a, b);
        if (r == 1) return -1;
        if (r == 0) {
            int r2 = b->ty->lt ? b->ty->lt(b, a) : -1;
            if (r2 == 1) return 1;
            if (r2 == 0) return 0;
        }
    }
    if (a && b && a->ty->tag == b->ty->tag) {
        switch (a->ty->tag) {
        case FT_STR: return fpy_str_cmp((FpyStr *)a, (FpyStr *)b);
        case FT_TUPLE: return fpy_tuple_cmp((FpyTuple *)a, (FpyTuple *)b);
        case FT_LIST: {
            FpyList *x = (FpyList *)a, *y = (FpyList *)b;
            int64_t n = x->len < y->len ? x->len : y->len;
            for (int64_t i = 0; i < n; i++) {
                int c = fpy_cmp_word(x->items[i], y->items[i], x->kind);
                if (c) return c;
            }
            return x->len < y->len ? -1 : (x->len > y->len);
        }
        case FT_BYTES: {
            FpyBytes *x = (FpyBytes *)a, *y = (FpyBytes *)b;
            int64_t n = x->len < y->len ? x->len : y->len;
            int c = memcmp(x->data, y->data, (size_t)n);
            if (c) return c < 0 ? -1 : 1;
            return x->len < y->len ? -1 : (x->len > y->len);
        }
        }
    }
    fpy_raise_fmt(FPY_EXC_TYPE, "'<' not supported between instances of '%s' and '%s'", fpy_type_name(a), fpy_type_name(b));
}

int fpy_cmp_word(fpy_word a, fpy_word b, int kind) {
    switch (kind) {
    case FK_INT:
    case FK_BOOL: return (fpy_int)a < (fpy_int)b ? -1 : ((fpy_int)a > (fpy_int)b);
    case FK_FLOAT: {
        double x = fpy_fw(a), y = fpy_fw(b);
        return x < y ? -1 : (x > y);
    }
    default: return fpy_obj_cmp(fpy_pw(a), fpy_pw(b));
    }
}

/* ================================================================== I/O */
static char *outbuf;

void fpy_write(FpyStr *s) { fwrite(s->data, 1, (size_t)s->len, stdout); }
void fpy_write_err(FpyStr *s) {
    fflush(stdout);
    fwrite(s->data, 1, (size_t)s->len, stderr);
}
void fpy_flush(void) { fflush(stdout); }

void fpy_print_str(FpyStr *s, FpyStr *end) {
    fwrite(s->data, 1, (size_t)s->len, stdout);
    if (end) fwrite(end->data, 1, (size_t)end->len, stdout);
    else fputc('\n', stdout);
}

FpyStr *fpy_input(FpyStr *prompt) {
    if (prompt) { fpy_write(prompt); fflush(stdout); }
    FpySB sb;
    fpy_sb_init(&sb);
    int c;
    bool any = false;
    while ((c = getchar()) != EOF) {
        any = true;
        if (c == '\n') break;
        char ch = (char)c;
        fpy_sb_add(&sb, &ch, 1);
    }
    if (!any) { free(sb.buf); fpy_raise_code(FPY_EXC_EOF, "EOF when reading a line"); }
    if (sb.len && sb.buf[sb.len - 1] == '\r') sb.len--;
    return fpy_sb_finish(&sb);
}

_Noreturn void fpy_exit(fpy_int code) {
    fflush(stdout);
    fflush(stderr);
    exit((int)code);
}

FpyStr *fpy_getenv(FpyStr *name, FpyStr *dflt) {
    const char *v = getenv(name->data);
    return v ? fpy_str_from_cstr(v) : dflt;
}

FpyStr *fpy_read_file(FpyStr *path) {
    FILE *f = fopen(path->data, "rb");
    if (!f) {
        int code = errno == ENOENT ? FPY_EXC_FILENOTFOUND : FPY_EXC_OS;
        fpy_raise_fmt(code, "[Errno %d] %s: '%s'", errno, strerror(errno), path->data);
    }
    FpySB sb;
    fpy_sb_init(&sb);
    char buf[65536];
    size_t n;
    while ((n = fread(buf, 1, sizeof buf, f)) > 0) fpy_sb_add(&sb, buf, (int64_t)n);
    fclose(f);
    return fpy_sb_finish(&sb);
}

void fpy_write_file(FpyStr *path, FpyStr *data, bool append) {
    FILE *f = fopen(path->data, append ? "ab" : "wb");
    if (!f) fpy_raise_fmt(FPY_EXC_OS, "[Errno %d] %s: '%s'", errno, strerror(errno), path->data);
    fwrite(data->data, 1, (size_t)data->len, f);
    fclose(f);
}

bool fpy_path_exists(FpyStr *path) {
    struct stat st;
    return stat(path->data, &st) == 0;
}

FpyList *fpy_get_argv(void) { return fpy_argv; }

FpyStr *fpy_platform(void) {
#if defined(__APPLE__)
    return fpy_str_from_cstr("darwin");
#elif defined(__linux__)
    return fpy_str_from_cstr("linux");
#else
    return fpy_str_from_cstr("unknown");
#endif
}

extern char **environ;
FpyDict *fpy_environ(void) {
    FpyDict *d = fpy_dict_new(FK_OBJ, FK_OBJ);
    for (char **e = environ; e && *e; e++) {
        const char *eq = strchr(*e, '=');
        if (!eq) continue;
        fpy_dict_set(d, fpy_wp(fpy_str_new(*e, eq - *e)), fpy_wp(fpy_str_from_cstr(eq + 1)));
    }
    return d;
}

_Noreturn static void os_error(const char *path) {
    int code = errno == ENOENT ? FPY_EXC_FILENOTFOUND : FPY_EXC_OS;
    fpy_raise_fmt(code, "[Errno %d] %s: '%s'", errno, strerror(errno), path);
}

FpyStr *fpy_getcwd(void) {
    char buf[4096];
    if (!getcwd(buf, sizeof buf)) os_error(".");
    return fpy_str_from_cstr(buf);
}
fpy_int fpy_getpid(void) { return (fpy_int)getpid(); }
FpyList *fpy_listdir(FpyStr *p) {
    DIR *d = opendir(p->data);
    if (!d) os_error(p->data);
    FpyList *l = fpy_list_new(FK_OBJ, 8);
    struct dirent *e;
    while ((e = readdir(d))) {
        if (strcmp(e->d_name, ".") == 0 || strcmp(e->d_name, "..") == 0) continue;
        fpy_list_append(l, fpy_wp(fpy_str_from_cstr(e->d_name)));
    }
    closedir(d);
    return l;
}
void fpy_mkdir(FpyStr *p) { if (mkdir(p->data, 0777) != 0) os_error(p->data); }
void fpy_remove(FpyStr *p) { if (unlink(p->data) != 0) os_error(p->data); }
void fpy_rmdir(FpyStr *p) { if (rmdir(p->data) != 0) os_error(p->data); }
void fpy_chdir(FpyStr *p) { if (chdir(p->data) != 0) os_error(p->data); }
fpy_int fpy_cpu_count(void) { long n = sysconf(_SC_NPROCESSORS_ONLN); return n > 0 ? n : 1; }
bool fpy_path_isfile(FpyStr *p) { struct stat st; return stat(p->data, &st) == 0 && S_ISREG(st.st_mode); }
bool fpy_path_isdir(FpyStr *p) { struct stat st; return stat(p->data, &st) == 0 && S_ISDIR(st.st_mode); }

double fpy_time_now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
}
double fpy_perf_counter(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
}
fpy_int fpy_time_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    return (fpy_int)ts.tv_sec * 1000000000 + ts.tv_nsec;
}
void fpy_sleep(double secs) {
    if (secs <= 0) return;
    struct timespec ts = {(time_t)secs, (long)((secs - (double)(time_t)secs) * 1e9)};
    nanosleep(&ts, NULL);
}

/* xoshiro256** */
static uint64_t rng[4] = {0x9E3779B97F4A7C15ull, 0xBF58476D1CE4E5B9ull, 0x94D049BB133111EBull, 0x2545F4914F6CDD1Dull};
static inline uint64_t rotl(uint64_t x, int k) { return (x << k) | (x >> (64 - k)); }
fpy_int fpy_random_bits(void) {
    uint64_t r = rotl(rng[1] * 5, 7) * 9, t = rng[1] << 17;
    rng[2] ^= rng[0]; rng[3] ^= rng[1]; rng[1] ^= rng[2]; rng[0] ^= rng[3];
    rng[2] ^= t; rng[3] = rotl(rng[3], 45);
    return (fpy_int)(r >> 1);
}
void fpy_random_seed(fpy_int seed) {
    uint64_t z = (uint64_t)seed;
    for (int i = 0; i < 4; i++) {
        z += 0x9E3779B97F4A7C15ull;
        uint64_t x = z;
        x = (x ^ (x >> 30)) * 0xBF58476D1CE4E5B9ull;
        x = (x ^ (x >> 27)) * 0x94D049BB133111EBull;
        rng[i] = x ^ (x >> 31);
    }
}
double fpy_random_float(void) { return (double)((uint64_t)fpy_random_bits() >> 10) * (1.0 / 9007199254740992.0); }
fpy_int fpy_random_range(fpy_int lo, fpy_int hi) {
    if (hi <= lo) fpy_raise_code(FPY_EXC_VALUE, "empty range for randrange()");
    uint64_t span = (uint64_t)(hi - lo);
    return lo + (fpy_int)((uint64_t)fpy_random_bits() % span);
}

double fpy_round(double v, fpy_int nd) {
    if (isnan(v) || isinf(v)) return v;
    if (nd >= 0 && nd <= 300) {
        /* printf rounds the exact binary value correctly (half-even), matching CPython */
        char buf[400];
        snprintf(buf, sizeof buf, "%.*f", (int)nd, v);
        return strtod(buf, NULL);
    }
    double p = pow(10.0, (double)nd);
    double x = v * p, r = nearbyint(x); /* banker's rounding under default FE_TONEAREST */
    return r / p;
}
fpy_int fpy_round_int(double v) {
    if (isnan(v) || isinf(v)) fpy_raise_code(FPY_EXC_VALUE, "cannot convert float NaN or infinity to integer");
    return (fpy_int)nearbyint(v);
}

/* ================================================================== dynamic (Any) operations */
static FpyObj *dyn_method(FpyObj *o, const char *name, FpyObj **args, int64_t n, bool *found) {
    for (const FpyType *t = o ? o->ty : NULL; t; t = t->base)
        for (int32_t i = 0; i < t->nmethods; i++)
            if (strcmp(t->methods[i].name, name) == 0) {
                FpyObj *argv[16];
                argv[0] = o;
                for (int64_t k = 0; k < n && k < 15; k++) argv[k + 1] = args[k];
                *found = true;
                return t->methods[i].fn(NULL, argv, n + 1);
            }
    *found = false;
    return NULL;
}

FpyObj *fpy_dyn_binop(int op, FpyObj *a, FpyObj *b) {
    if (fpy_is_py(a) || fpy_is_py(b)) return fpy_py->binop(op, a, b);
    /* op: 0 add 1 sub 2 mul 3 truediv 4 floordiv 5 mod 6 pow 7 lshift 8 rshift 9 and 10 or 11 xor */
    static const char *names[] = {"__add__", "__sub__", "__mul__", "__truediv__", "__floordiv__", "__mod__", "__pow__",
                                  "__lshift__", "__rshift__", "__and__", "__or__", "__xor__"};
    if (is_num(a) && is_num(b)) {
        bool fl = a->ty == &fpy_type_float || b->ty == &fpy_type_float;
        if (!fl && op != 3) {
            fpy_int x = (fpy_int)((FpyBox *)a)->v, y = (fpy_int)((FpyBox *)b)->v;
            switch (op) {
            case 0: return fpy_box_int(fpy_add(x, y));
            case 1: return fpy_box_int(fpy_sub(x, y));
            case 2: return fpy_box_int(fpy_mul(x, y));
            case 4: return fpy_box_int(fpy_floordiv(x, y));
            case 5: return fpy_box_int(fpy_mod(x, y));
            case 6: return y < 0 ? fpy_box_float(pow((double)x, (double)y)) : fpy_box_int(fpy_ipow(x, y));
            case 7: return fpy_box_int(fpy_lshift(x, y));
            case 8: return fpy_box_int(fpy_rshift(x, y));
            case 9: return a->ty == &fpy_type_bool && b->ty == &fpy_type_bool ? fpy_box_bool(x & y) : fpy_box_int(x & y);
            case 10: return a->ty == &fpy_type_bool && b->ty == &fpy_type_bool ? fpy_box_bool(x | y) : fpy_box_int(x | y);
            case 11: return a->ty == &fpy_type_bool && b->ty == &fpy_type_bool ? fpy_box_bool(x ^ y) : fpy_box_int(x ^ y);
            }
        }
        double x = num_val(a), y = num_val(b);
        switch (op) {
        case 0: return fpy_box_float(x + y);
        case 1: return fpy_box_float(x - y);
        case 2: return fpy_box_float(x * y);
        case 3: return fpy_box_float(fpy_truediv(x, y));
        case 4: return fpy_box_float(fpy_ffloordiv(x, y));
        case 5: return fpy_box_float(fpy_fmod(x, y));
        case 6: return fpy_box_float(pow(x, y));
        }
    }
    if (a && b && a->ty == b->ty) {
        switch (a->ty->tag) {
        case FT_STR:
            if (op == 0) return (FpyObj *)fpy_str_concat((FpyStr *)a, (FpyStr *)b);
            break;
        case FT_LIST:
            if (op == 0) return (FpyObj *)fpy_list_concat((FpyList *)a, (FpyList *)b);
            break;
        case FT_BYTES:
            if (op == 0) return (FpyObj *)fpy_bytes_concat((FpyBytes *)a, (FpyBytes *)b);
            break;
        case FT_SET:
            if (op == 10) return (FpyObj *)fpy_set_union((FpySet *)a, (FpySet *)b);
            if (op == 9) return (FpyObj *)fpy_set_intersection((FpySet *)a, (FpySet *)b);
            if (op == 1) return (FpyObj *)fpy_set_difference((FpySet *)a, (FpySet *)b);
            if (op == 11) return (FpyObj *)fpy_set_symdiff((FpySet *)a, (FpySet *)b);
            break;
        }
    }
    if (op == 2 && a && b) {
        if (a->ty == &fpy_type_str && b->ty == &fpy_type_int) return (FpyObj *)fpy_str_repeat((FpyStr *)a, fpy_unbox_int(b));
        if (a->ty == &fpy_type_list && b->ty == &fpy_type_int) return (FpyObj *)fpy_list_repeat((FpyList *)a, fpy_unbox_int(b));
    }
    if (op == 5 && a && a->ty == &fpy_type_str) fpy_raise_code(FPY_EXC_NOTIMPL, "%-formatting is not supported; use f-strings");
    bool found;
    FpyObj *r = dyn_method(a, names[op], &b, 1, &found);
    if (found) return r;
    fpy_raise_fmt(FPY_EXC_TYPE, "unsupported operand type(s) for %s: '%s' and '%s'", names[op], fpy_type_name(a), fpy_type_name(b));
}

FpyObj *fpy_dyn_neg(FpyObj *a) {
    if (a && a->ty == &fpy_type_float) return fpy_box_float(-fpy_unbox_float(a));
    if (a && (a->ty == &fpy_type_int || a->ty == &fpy_type_bool)) return fpy_box_int(fpy_neg(fpy_unbox_int(a)));
    bool found;
    FpyObj *r = dyn_method(a, "__neg__", NULL, 0, &found);
    if (found) return r;
    fpy_raise_fmt(FPY_EXC_TYPE, "bad operand type for unary -: '%s'", fpy_type_name(a));
}

bool fpy_dyn_truthy(FpyObj *a) {
    if (!a) return false;
    if (fpy_is_py(a)) return fpy_py->truthy(a);
    switch (a->ty->tag) {
    case FT_BOOL:
    case FT_INT: return ((FpyBox *)a)->v != 0;
    case FT_FLOAT: return fpy_unbox_float(a) != 0.0;
    case FT_STR: return ((FpyStr *)a)->len != 0;
    case FT_BYTES: return ((FpyBytes *)a)->len != 0;
    case FT_LIST: return ((FpyList *)a)->len != 0;
    case FT_DICT:
    case FT_SET: return ((FpyDict *)a)->len != 0;
    case FT_TUPLE: return ((FpyTuple *)a)->len != 0;
    }
    bool found;
    FpyObj *r = dyn_method(a, "__bool__", NULL, 0, &found);
    if (found) return fpy_dyn_truthy(r);
    r = dyn_method(a, "__len__", NULL, 0, &found);
    if (found) return fpy_unbox_int(r) != 0;
    return true;
}

int fpy_dyn_compare(int op, FpyObj *a, FpyObj *b) {
    if (fpy_is_py(a) || fpy_is_py(b)) return fpy_py->compare(op, a, b);
    /* op: 0 eq 1 ne 2 lt 3 le 4 gt 5 ge */
    if (op == 0) return fpy_obj_eq(a, b);
    if (op == 1) return !fpy_obj_eq(a, b);
    int c = fpy_obj_cmp(a, b);
    switch (op) {
    case 2: return c < 0;
    case 3: return c <= 0;
    case 4: return c > 0;
    default: return c >= 0;
    }
}

FpyObj *fpy_dyn_getattr(FpyObj *o, const char *name) {
    if (fpy_is_py(o)) return fpy_py->getattr(o, name);
    if (!o) fpy_raise_fmt(FPY_EXC_ATTRIBUTE, "'NoneType' object has no attribute '%s'", name);
    for (const FpyType *t = o->ty; t; t = t->base)
        for (int32_t i = 0; i < t->nfields; i++)
            if (strcmp(t->fields[i].name, name) == 0) {
                fpy_word w = *(fpy_word *)((char *)o + t->fields[i].offset);
                if (t->fields[i].kind == FK_BOOL) w = *(bool *)((char *)o + t->fields[i].offset);
                return fpy_box_word(w, t->fields[i].kind);
            }
    for (const FpyType *t = o->ty; t; t = t->base)
        for (int32_t i = 0; i < t->nmethods; i++)
            if (strcmp(t->methods[i].name, name) == 0) {
                FpyFunc *f = fpy_func_new(NULL, NULL, t->methods[i].fn, t->methods[i].name);
                f->self = o;
                return (FpyObj *)f;
            }
    fpy_raise_fmt(FPY_EXC_ATTRIBUTE, "'%s' object has no attribute '%s'", fpy_type_name(o), name);
}

FpyObj *fpy_dyn_getattr_default(FpyObj *o, const char *name, FpyObj *dflt) {
    FpyExcFrame f;
    FpyObj *volatile r = dflt;
    if (FPY_TRY(f)) {
        r = fpy_dyn_getattr(o, name);
        FPY_POP(f);
    } else {
        FPY_POP(f);
        if (!fpy_exc_match(fpy_cur_exc, fpy_builtin_exc_type(FPY_EXC_ATTRIBUTE))) fpy_reraise();
        r = dflt;
    }
    return r;
}

/* next(x) on a dynamic value: native iterators step directly (boxing the item). */
FpyObj *fpy_dyn_next(FpyObj *it) {
    if (it && !fpy_is_py(it) && it->ty->tag == FT_ITER) {
        FpyIter *in = (FpyIter *)it;
        fpy_word w;
        if (!in->next(in, &w)) fpy_raise_stop_iteration();
        return fpy_box_word(w, in->kind);
    }
    return fpy_dyn_call_method(it, "__next__", NULL, 0);
}

FpyObj *fpy_dyn_next_default(FpyObj *it, FpyObj *dflt) {
    FpyExcFrame f;
    FpyObj *volatile r = dflt;
    if (FPY_TRY(f)) {
        r = fpy_dyn_next(it);
        FPY_POP(f);
    } else {
        FPY_POP(f);
        if (!fpy_exc_match(fpy_cur_exc, fpy_builtin_exc_type(FPY_EXC_STOPITER))) fpy_reraise();
        r = dflt;
    }
    return r;
}

bool fpy_dyn_hasattr(FpyObj *o, const char *name) {
    FpyExcFrame f;
    volatile bool ok = true;
    if (FPY_TRY(f)) {
        (void)fpy_dyn_getattr(o, name);
        FPY_POP(f);
    } else {
        FPY_POP(f);
        if (!fpy_exc_match(fpy_cur_exc, fpy_builtin_exc_type(FPY_EXC_ATTRIBUTE))) fpy_reraise();
        ok = false;
    }
    return ok;
}

void fpy_dyn_setattr(FpyObj *o, const char *name, FpyObj *v) {
    if (fpy_is_py(o)) { fpy_py->setattr(o, name, v); return; }
    if (!o) fpy_raise_fmt(FPY_EXC_ATTRIBUTE, "'NoneType' object has no attribute '%s'", name);
    for (const FpyType *t = o->ty; t; t = t->base)
        for (int32_t i = 0; i < t->nfields; i++)
            if (strcmp(t->fields[i].name, name) == 0) {
                char *p = (char *)o + t->fields[i].offset;
                int k = t->fields[i].kind;
                if (k == FK_BOOL) *(bool *)p = fpy_unbox_word(v, k, "bool") != 0;
                else *(fpy_word *)p = fpy_unbox_word(v, k, k == FK_INT ? "int" : k == FK_FLOAT ? "float" : "object");
                return;
            }
    fpy_raise_fmt(FPY_EXC_ATTRIBUTE, "'%s' object has no attribute '%s'", fpy_type_name(o), name);
}

FpyObj *fpy_dyn_getitem(FpyObj *o, FpyObj *k) {
    if (fpy_is_py(o)) return fpy_py->getitem(o, k);
    if (o) switch (o->ty->tag) {
    case FT_LIST: {
        FpyList *l = (FpyList *)o;
        return fpy_box_word(fpy_list_get(l, fpy_dyn_to_int(k)), l->kind);
    }
    case FT_TUPLE: {
        FpyTuple *t = (FpyTuple *)o;
        int64_t i = fpy_norm_index(fpy_dyn_to_int(k), t->len, "tuple");
        return fpy_box_word(t->items[i], t->kinds[i]);
    }
    case FT_STR: return (FpyObj *)fpy_str_getitem((FpyStr *)o, fpy_dyn_to_int(k));
    case FT_DICT: {
        FpyDict *d = (FpyDict *)o;
        return fpy_box_word(fpy_dict_get(d, fpy_unbox_word(k, d->kkind, "key")), d->vkind);
    }
    }
    bool found;
    FpyObj *r = dyn_method(o, "__getitem__", &k, 1, &found);
    if (found) return r;
    fpy_raise_fmt(FPY_EXC_TYPE, "'%s' object is not subscriptable", fpy_type_name(o));
}

void fpy_dyn_setitem(FpyObj *o, FpyObj *k, FpyObj *v) {
    if (fpy_is_py(o)) { fpy_py->setitem(o, k, v); return; }
    if (o) switch (o->ty->tag) {
    case FT_LIST: {
        FpyList *l = (FpyList *)o;
        fpy_list_set(l, fpy_dyn_to_int(k), fpy_unbox_word(v, l->kind, "list element"));
        return;
    }
    case FT_DICT: {
        FpyDict *d = (FpyDict *)o;
        fpy_dict_set(d, fpy_unbox_word(k, d->kkind, "key"), fpy_unbox_word(v, d->vkind, "value"));
        return;
    }
    }
    FpyObj *args[2] = {k, v};
    bool found;
    dyn_method(o, "__setitem__", args, 2, &found);
    if (!found) fpy_raise_fmt(FPY_EXC_TYPE, "'%s' object does not support item assignment", fpy_type_name(o));
}

FpyObj *fpy_dyn_call(FpyObj *f, FpyObj **args, int64_t nargs) {
    if (fpy_is_py(f)) return fpy_py->call(f, args, nargs, NULL, NULL, 0);
    if (f && f->ty == &fpy_type_func) {
        FpyFunc *fn = (FpyFunc *)f;
        if (!fn->dyn) fpy_raise_code(FPY_EXC_TYPE, "function cannot be called dynamically");
        if (fn->self) {
            FpyObj *argv[32];
            if (nargs > 31) fpy_raise_code(FPY_EXC_TYPE, "too many arguments");
            argv[0] = fn->self;
            for (int64_t i = 0; i < nargs; i++) argv[i + 1] = args[i];
            return fn->dyn(fn->env, argv, nargs + 1);
        }
        return fn->dyn(fn->env, args, nargs);
    }
    if (f && f->ty == &fpy_type_class) {
        const FpyType *cls = ((FpyClassObj *)f)->cls;
        if (cls->ctor) return cls->ctor(NULL, args, nargs);
    }
    bool found;
    FpyObj *r = dyn_method(f, "__call__", args, nargs, &found);
    if (found) return r;
    fpy_raise_fmt(FPY_EXC_TYPE, "'%s' object is not callable", fpy_type_name(f));
}

static FpyStr *arg_str(FpyObj **a, int64_t n, int64_t i, const char *m) {
    if (i >= n) fpy_raise_fmt(FPY_EXC_TYPE, "%s() missing argument", m);
    if (!a[i] || a[i]->ty != &fpy_type_str) fpy_raise_fmt(FPY_EXC_TYPE, "%s() expected str, got %s", m, fpy_type_name(a[i]));
    return (FpyStr *)a[i];
}
static FpyStr *opt_str(FpyObj **a, int64_t n, int64_t i) {
    if (i >= n || !a[i]) return NULL;
    if (a[i]->ty != &fpy_type_str) fpy_raise_fmt(FPY_EXC_TYPE, "expected str, got %s", fpy_type_name(a[i]));
    return (FpyStr *)a[i];
}
static fpy_int opt_int(FpyObj **a, int64_t n, int64_t i, fpy_int d) { return i < n ? fpy_dyn_to_int(a[i]) : d; }

/* Dynamic method calls on native builtin values (str/list/dict/set). Returns true if handled. */
static bool dyn_builtin_method(FpyObj *o, const char *m, FpyObj **a, int64_t n, FpyObj **out) {
#define IS(x) (strcmp(m, x) == 0)
#define RET(v) do { *out = (FpyObj *)(v); return true; } while (0)
    if (!o) return false;
    switch (o->ty->tag) {
    case FT_STR: {
        FpyStr *s = (FpyStr *)o;
        if (IS("encode")) RET(fpy_str_encode(s));
        if (IS("upper")) RET(fpy_str_upper(s));
        if (IS("lower")) RET(fpy_str_lower(s));
        if (IS("strip")) RET(fpy_str_strip(s, opt_str(a, n, 0)));
        if (IS("lstrip")) RET(fpy_str_lstrip(s, opt_str(a, n, 0)));
        if (IS("rstrip")) RET(fpy_str_rstrip(s, opt_str(a, n, 0)));
        if (IS("split")) RET(fpy_str_split(s, opt_str(a, n, 0), opt_int(a, n, 1, -1)));
        if (IS("splitlines")) RET(fpy_str_splitlines(s, false));
        if (IS("replace")) RET(fpy_str_replace(s, arg_str(a, n, 0, m), arg_str(a, n, 1, m), opt_int(a, n, 2, -1)));
        if (IS("startswith")) RET(fpy_box_bool(fpy_str_startswith(s, arg_str(a, n, 0, m))));
        if (IS("endswith")) RET(fpy_box_bool(fpy_str_endswith(s, arg_str(a, n, 0, m))));
        if (IS("find")) RET(fpy_box_int(fpy_str_find(s, arg_str(a, n, 0, m))));
        if (IS("count")) RET(fpy_box_int(fpy_str_count(s, arg_str(a, n, 0, m))));
        if (IS("join")) {
            FpyList *l = fpy_list_from_iter(FK_OBJ, fpy_dyn_iter(n ? a[0] : NULL));
            RET(fpy_str_join(s, l));
        }
        if (IS("isdigit")) RET(fpy_box_bool(fpy_str_isdigit(s)));
        if (IS("title")) RET(fpy_str_title(s));
        if (IS("capitalize")) RET(fpy_str_capitalize(s));
        break;
    }
    case FT_LIST: {
        FpyList *l = (FpyList *)o;
        if (IS("append")) { fpy_list_append(l, fpy_unbox_word(n ? a[0] : NULL, l->kind, "list element")); RET(NULL); }
        if (IS("extend")) { fpy_list_extend_iter(l, fpy_dyn_iter(n ? a[0] : NULL)); RET(NULL); }
        if (IS("pop")) RET(fpy_box_word(fpy_list_pop(l, opt_int(a, n, 0, -1)), l->kind));
        if (IS("insert")) { fpy_list_insert(l, opt_int(a, n, 0, 0), fpy_unbox_word(n > 1 ? a[1] : NULL, l->kind, "list element")); RET(NULL); }
        if (IS("remove")) { fpy_list_remove(l, fpy_unbox_word(n ? a[0] : NULL, l->kind, "list element")); RET(NULL); }
        if (IS("index")) RET(fpy_box_int(fpy_list_index(l, fpy_unbox_word(n ? a[0] : NULL, l->kind, "list element"))));
        if (IS("count")) RET(fpy_box_int(fpy_list_count(l, fpy_unbox_word(n ? a[0] : NULL, l->kind, "list element"))));
        if (IS("copy")) RET(fpy_list_copy(l));
        if (IS("reverse")) { fpy_list_reverse(l); RET(NULL); }
        if (IS("clear")) { fpy_list_clear(l); RET(NULL); }
        if (IS("sort")) { fpy_list_sort(l, false); RET(NULL); }
        break;
    }
    case FT_DICT: {
        FpyDict *d = (FpyDict *)o;
        if (IS("get")) {
            fpy_word v;
            if (fpy_dict_lookup(d, fpy_unbox_word(n ? a[0] : NULL, d->kkind, "key"), &v)) RET(fpy_box_word(v, d->vkind));
            RET(n > 1 ? a[1] : NULL);
        }
        if (IS("keys")) RET(fpy_dict_keys(d));
        if (IS("values")) RET(fpy_dict_values(d));
        if (IS("items")) RET(fpy_dict_items(d));
        if (IS("pop")) {
            fpy_word k = fpy_unbox_word(n ? a[0] : NULL, d->kkind, "key");
            fpy_word v;
            if (fpy_dict_lookup(d, k, &v)) { fpy_dict_del(d, k, false); RET(fpy_box_word(v, d->vkind)); }
            if (n > 1) RET(a[1]);
            fpy_dict_get(d, k);
        }
        if (IS("setdefault")) RET(fpy_box_word(fpy_dict_setdefault(d, fpy_unbox_word(n ? a[0] : NULL, d->kkind, "key"), fpy_unbox_word(n > 1 ? a[1] : NULL, d->vkind, "value")), d->vkind));
        if (IS("update")) {
            FpyObj *x = n ? a[0] : NULL;
            if (!x || x->ty->tag != FT_DICT) fpy_raise_code(FPY_EXC_TYPE, "dict.update() expects a dict");
            fpy_dict_update(d, fpy_retype_dict((FpyDict *)x, d->kkind, d->vkind));
            RET(NULL);
        }
        if (IS("copy")) RET(fpy_dict_copy(d));
        if (IS("clear")) { fpy_dict_clear(d); RET(NULL); }
        break;
    }
    case FT_SET: {
        FpySet *st = (FpySet *)o;
        if (IS("add")) { fpy_set_add(st, fpy_unbox_word(n ? a[0] : NULL, st->kkind, "element")); RET(NULL); }
        if (IS("discard")) { fpy_set_discard(st, fpy_unbox_word(n ? a[0] : NULL, st->kkind, "element")); RET(NULL); }
        if (IS("remove")) { fpy_set_remove(st, fpy_unbox_word(n ? a[0] : NULL, st->kkind, "element")); RET(NULL); }
        break;
    }
    }
    return false;
#undef IS
#undef RET
}

FpyObj *fpy_dyn_call_method(FpyObj *o, const char *name, FpyObj **args, int64_t nargs) {
    if (fpy_is_py(o)) return fpy_py->call_method(o, name, args, nargs, NULL, NULL, 0);
    bool found;
    FpyObj *r = dyn_method(o, name, args, nargs, &found);
    if (found) return r;
    if (dyn_builtin_method(o, name, args, nargs, &r)) return r;
    if (fpy_py && o && o->ty->tag != FT_INSTANCE) {
        /* other builtin-type methods: evaluate on a Python copy of the value */
        return fpy_py->call_method(o, name, args, nargs, NULL, NULL, 0);
    }
    return fpy_dyn_call(fpy_dyn_getattr(o, name), args, nargs);
}

fpy_int fpy_dyn_len(FpyObj *o) {
    if (fpy_is_py(o)) return fpy_py->len(o);
    if (o) switch (o->ty->tag) {
    case FT_STR: return ((FpyStr *)o)->nchars;
    case FT_BYTES: return ((FpyBytes *)o)->len;
    case FT_LIST: return ((FpyList *)o)->len;
    case FT_DICT:
    case FT_SET: return ((FpyDict *)o)->len;
    case FT_TUPLE: return ((FpyTuple *)o)->len;
    }
    bool found;
    FpyObj *r = dyn_method(o, "__len__", NULL, 0, &found);
    if (found) return fpy_dyn_to_int(r);
    fpy_raise_fmt(FPY_EXC_TYPE, "object of type '%s' has no len()", fpy_type_name(o));
}

static bool boxing_next(FpyIter *it, fpy_word *out) {
    FpyIter *in = (FpyIter *)it->o1;
    fpy_word w;
    if (!in->next(in, &w)) return false;
    *out = fpy_wp(fpy_box_word(w, in->kind));
    return true;
}

static bool dyn_obj_next(FpyIter *it, fpy_word *out) {
    FpyObj *r = fpy_dyn_call_method(it->o1, "__next__", NULL, 0);
    /* StopIteration is signalled by exception; the caller catches it in fpy_dyn_iter users */
    *out = fpy_wp(r);
    return true;
}

FpyIter *fpy_dyn_iter(FpyObj *o) {
    if (fpy_is_py(o)) return fpy_py->iter(o);
    FpyIter *in = NULL;
    if (o) switch (o->ty->tag) {
    case FT_LIST: in = fpy_list_iter((FpyList *)o); break;
    case FT_TUPLE: {
        FpyTuple *t = (FpyTuple *)o;
        FpyList *l = fpy_list_new(FK_OBJ, t->len);
        for (int64_t i = 0; i < t->len; i++) fpy_list_append(l, fpy_wp(fpy_box_word(t->items[i], t->kinds[i])));
        return fpy_list_iter(l);
    }
    case FT_STR: return fpy_str_iter((FpyStr *)o);
    case FT_DICT: in = fpy_dict_iter_keys((FpyDict *)o); break;
    case FT_SET: in = fpy_set_iter((FpySet *)o); break;
    case FT_ITER: in = (FpyIter *)o; break;
    }
    if (!in) {
        bool found;
        FpyObj *r = dyn_method(o, "__iter__", NULL, 0, &found);
        if (!found) fpy_raise_fmt(FPY_EXC_TYPE, "'%s' object is not iterable", fpy_type_name(o));
        if (r && r->ty == &fpy_type_iter) in = (FpyIter *)r;
        else {
            FpyIter *it = fpy_iter_new(dyn_obj_next, FK_OBJ);
            it->o1 = r;
            return it;
        }
    }
    if (in->kind == FK_OBJ) return in;
    FpyIter *it = fpy_iter_new(boxing_next, FK_OBJ);
    it->o1 = (FpyObj *)in;
    return it;
}

static const char *kind_name(int kind) { return kind == FK_INT ? "int" : kind == FK_FLOAT ? "float" : "bool"; }

static bool unboxing_next(FpyIter *it, fpy_word *out) {
    FpyIter *in = (FpyIter *)it->o1;
    fpy_word w;
    if (!in->next(in, &w)) return false;
    *out = fpy_unbox_word(fpy_pw(w), it->kind, kind_name(it->kind));
    return true;
}

/* Iterate an Any value as Iterator[int|float|bool]: elements are unboxed (and checked). */
FpyIter *fpy_dyn_iter_kind(FpyObj *o, int kind) {
    FpyIter *in = fpy_dyn_iter(o);
    if (kind == FK_OBJ) return in;
    FpyIter *it = fpy_iter_new(unboxing_next, kind);
    it->o1 = (FpyObj *)in;
    return it;
}

bool fpy_dyn_contains(FpyObj *c, FpyObj *item) {
    if (fpy_is_py(c)) return fpy_py->contains(c, item);
    if (c) switch (c->ty->tag) {
    case FT_STR: return fpy_str_contains((FpyStr *)c, (FpyStr *)item);
    case FT_LIST: {
        FpyList *l = (FpyList *)c;
        for (int64_t i = 0; i < l->len; i++)
            if (fpy_obj_eq(fpy_box_word(l->items[i], l->kind), item)) return true;
        return false;
    }
    case FT_DICT:
    case FT_SET: {
        FpyDict *d = (FpyDict *)c;
        return fpy_dict_contains(d, fpy_unbox_word(item, d->kkind, "key"));
    }
    case FT_TUPLE: {
        FpyTuple *t = (FpyTuple *)c;
        for (int64_t i = 0; i < t->len; i++)
            if (fpy_obj_eq(fpy_box_word(t->items[i], t->kinds[i]), item)) return true;
        return false;
    }
    }
    bool found;
    FpyObj *r = dyn_method(c, "__contains__", &item, 1, &found);
    if (found) return fpy_dyn_truthy(r);
    fpy_raise_fmt(FPY_EXC_TYPE, "argument of type '%s' is not iterable", fpy_type_name(c));
}

fpy_int fpy_dyn_to_int(FpyObj *o) {
    if (o && (o->ty == &fpy_type_int || o->ty == &fpy_type_bool)) return fpy_unbox_int(o);
    fpy_raise_fmt(FPY_EXC_TYPE, "expected int, got %s", fpy_type_name(o));
}

double fpy_dyn_to_float(FpyObj *o) {
    if (is_num(o)) return num_val(o);
    fpy_raise_fmt(FPY_EXC_TYPE, "expected float, got %s", fpy_type_name(o));
}

FpyObj *fpy_dyn_call_kw(FpyObj *f, FpyObj **args, int64_t nargs, const char **kwn, FpyObj **kwv, int64_t nk) {
    if (fpy_is_py(f)) return fpy_py->call(f, args, nargs, kwn, kwv, nk);
    if (nk) fpy_raise_code(FPY_EXC_TYPE, "keyword arguments are not supported for this dynamic call");
    return fpy_dyn_call(f, args, nargs);
}

FpyObj *fpy_dyn_call_method_kw(FpyObj *o, const char *name, FpyObj **args, int64_t nargs, const char **kwn, FpyObj **kwv, int64_t nk) {
    if (fpy_is_py(o)) return fpy_py->call_method(o, name, args, nargs, kwn, kwv, nk);
    if (nk && fpy_py && o && o->ty->tag != FT_INSTANCE) return fpy_py->call_method(o, name, args, nargs, kwn, kwv, nk);
    if (nk) fpy_raise_code(FPY_EXC_TYPE, "keyword arguments are not supported for this dynamic call");
    return fpy_dyn_call_method(o, name, args, nargs);
}

/* type(e) for an exception value, as an Any value (class object or Python type). */
FpyObj *fpy_exc_type_of(FpyObj *e) {
    if (!e) return NULL;
    if (fpy_is_py(e)) return fpy_dyn_getattr(e, "__class__");
    return fpy_class_obj(e->ty);
}

FpyObj *fpy_exc_instance(FpyObj *o) {
    if (fpy_is_py(o)) return fpy_py->exc_instance(o);
    if (o && o->ty->tag == FT_CLASS && ((FpyClassObj *)o)->cls->ctor) return ((FpyClassObj *)o)->cls->ctor(NULL, NULL, 0);
    return o;
}

bool fpy_dyn_isinstance(FpyObj *o, FpyObj *cls) {
    if (cls && cls->ty->tag == FT_CLASS) return fpy_isinstance_t(o, ((FpyClassObj *)cls)->cls);
    if (fpy_is_py(cls)) return fpy_py->isinstance(o, cls);
    return false;
}

bool fpy_exc_match(FpyObj *e, const FpyType *t) {
    if (fpy_isinstance_t(e, t)) return true;
    return fpy_is_py(e) && fpy_py->exc_matches_native(e, t);
}

bool fpy_exc_match_dyn(FpyObj *e, FpyObj *cls) {
    if (cls && cls->ty->tag == FT_CLASS) return fpy_isinstance_t(e, ((FpyClassObj *)cls)->cls);
    if (fpy_py) return fpy_py->exc_matches_dyn(e, cls);
    return false;
}

/* A Python exception caught by a native `except T as e` handler becomes a native T. */
FpyObj *fpy_exc_as(FpyObj *e, const FpyType *t) {
    if (fpy_isinstance_t(e, t)) return e;
    if (t->ctor) {
        FpyObj *msg = (FpyObj *)fpy_obj_str(e);
        return t->ctor(NULL, &msg, 1);
    }
    return e;
}

fpy_int fpy_float_to_int(double v) {
    if (isnan(v)) fpy_raise_code(FPY_EXC_VALUE, "cannot convert float NaN to integer");
    if (isinf(v)) fpy_raise_code(FPY_EXC_OVERFLOW, "cannot convert float infinity to integer");
    if (v >= 9223372036854775808.0 || v < -9223372036854775808.0) fpy_overflow();
    return (fpy_int)v;
}

fpy_int fpy_dyn_int(FpyObj *o) {
    if (o && o->ty == &fpy_type_float) return fpy_float_to_int(fpy_unbox_float(o));
    if (o && o->ty == &fpy_type_str) return fpy_str_to_int((FpyStr *)o, 10);
    return fpy_dyn_to_int(o);
}

double fpy_dyn_float(FpyObj *o) {
    if (o && o->ty == &fpy_type_str) return fpy_str_to_float((FpyStr *)o);
    return fpy_dyn_to_float(o);
}

/* float.hex(), in CPython's exact format: [-]0x1.<13 hex digits>p<exp> */
FpyStr *fpy_float_hex(double v) {
    char buf[64];
    int n;
    if (isnan(v)) n = snprintf(buf, sizeof buf, "nan");
    else if (isinf(v)) n = snprintf(buf, sizeof buf, "%sinf", v < 0 ? "-" : "");
    else {
        uint64_t bits;
        memcpy(&bits, &v, sizeof bits);
        const char *sign = bits >> 63 ? "-" : "";
        int exp = (int)((bits >> 52) & 0x7FF);
        uint64_t mant = bits & ((UINT64_C(1) << 52) - 1);
        if (exp == 0 && mant == 0) n = snprintf(buf, sizeof buf, "%s0x0.0p+0", sign);
        else if (exp == 0) n = snprintf(buf, sizeof buf, "%s0x0.%013llxp-1022", sign, (unsigned long long)mant);
        else n = snprintf(buf, sizeof buf, "%s0x1.%013llxp%+d", sign, (unsigned long long)mant, exp - 1023);
    }
    return fpy_str_new(buf, n);
}

FpyObj *fpy_check_cast(FpyObj *o, const FpyType *t, const char *tyname) {
    if (!fpy_isinstance_t(o, t)) fpy_raise_fmt(FPY_EXC_TYPE, "cast to %s failed: got %s", tyname, fpy_type_name(o));
    return o;
}

/* ================================================================== init */
void fpy_rt_mark_roots(void) {
    if (fpy_cur_exc) fpy_gc_mark(fpy_cur_exc);
    if (fpy_argv) fpy_gc_mark(fpy_argv);
    for (int i = 0; i < 262; i++)
        if (small_ints[i]) fpy_gc_mark(small_ints[i]);
    fpy_str_mark_cache();
    fpy_gen_mark_roots();
    if (fpy_py) fpy_py->mark_roots();
}

void fpy_rt_init(void *stack_bottom, int argc, char **argv) {
    fpy_gc_init(stack_bottom);
    outbuf = malloc(1 << 16);
    setvbuf(stdout, outbuf, _IOFBF, 1 << 16);
    if (isatty(1)) setvbuf(stdout, outbuf, _IOLBF, 1 << 16);
    fpy_str_init_cache();
    for (int i = 0; i < 262; i++) {
        FpyBox *b = fpy_alloc(&fpy_type_int, sizeof(FpyBox));
        b->v = (fpy_word)(fpy_int)(i - 5);
        small_ints[i] = b;
    }
    fpy_argv = fpy_list_new(FK_OBJ, argc);
    for (int i = 0; i < argc; i++) fpy_list_append(fpy_argv, fpy_wp(fpy_str_from_cstr(argv[i])));
    fpy_random_seed((fpy_int)fpy_time_ns() ^ (fpy_int)getpid());
}

/* Runs the program entry; handles uncaught exceptions and SystemExit. */
/* Fatal signals (a runtime bug, or a stack overflow): keep what the program already printed,
 * name the signal, then die with the default action so the exit status still says why. */
static void fatal_signal(int sig) {
    fflush(stdout);
    const char *msg = sig == SIGSEGV ? "fatal error: segmentation fault (stack overflow or fpython runtime bug)\n"
                    : sig == SIGBUS  ? "fatal error: bus error (fpython runtime bug)\n"
                    : sig == SIGFPE  ? "fatal error: floating point exception\n"
                                     : "fatal error: illegal instruction\n";
    ssize_t w = write(2, msg, strlen(msg));
    (void)w;
    signal(sig, SIG_DFL);
    raise(sig);
}

static void install_fatal_handlers(void) {
    static char altstack[1 << 16];
    stack_t ss = {.ss_sp = altstack, .ss_size = sizeof altstack, .ss_flags = 0};
    sigaltstack(&ss, NULL);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = fatal_signal;
    sa.sa_flags = SA_ONSTACK;
    sigemptyset(&sa.sa_mask);
    int sigs[] = {SIGSEGV, SIGBUS, SIGFPE, SIGILL};
    for (size_t i = 0; i < sizeof sigs / sizeof sigs[0]; i++) sigaction(sigs[i], &sa, NULL);
}

int fpy_run_main(void (*entry)(void)) {
    install_fatal_handlers();
    FpyExcFrame f;
    if (FPY_TRY(f)) {
        entry();
        FPY_POP(f);
    } else {
        FPY_POP(f);
        FpyObj *e = fpy_cur_exc;
        if (fpy_is_py(e)) {
            bool is_exit = false;
            int code = fpy_py->exit_code(e, &is_exit);
            if (is_exit) {
                fflush(stdout);
                return code;
            }
        }
        if (fpy_isinstance_t(e, fpy_builtin_exc_type(FPY_EXC_SYSTEMEXIT))) {
            fflush(stdout);
            FpyStr *msg = fpy_obj_str(e);
            fpy_int code = 0;
            if (msg->len) {
                char *end;
                long long v = strtoll(msg->data, &end, 10);
                if (*end == 0) code = v;
                else { fprintf(stderr, "%s\n", msg->data); code = 1; }
            }
            return (int)code;
        }
        print_uncaught(e);
        fflush(stdout);
        return 1;
    }
    fflush(stdout);
    return 0;
}
