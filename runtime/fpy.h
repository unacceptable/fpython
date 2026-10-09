/* fpython runtime — core definitions.
 * Licensed under the PSF License Agreement (see LICENSE).
 *
 * Every fpython value fits in one 64-bit word:
 *   int   -> int64_t        float -> double       bool -> bool (int64 in containers)
 *   None  -> NULL           everything else -> pointer to a GC object with an FpyType* header.
 * Containers remember the storage kind of their elements (FK_*), which lets the runtime
 * print, hash, compare and trace them without monomorphized helpers.
 */
#ifndef FPY_H
#define FPY_H

#include <stdint.h>
#include <stdbool.h>
#include <stddef.h>
#include <string.h>
#include <stdio.h>
#include <stdlib.h>
#include <setjmp.h>
#include <math.h>

typedef int64_t fpy_int;
typedef double fpy_float;
typedef uint64_t fpy_word;

typedef struct FpyType FpyType;
typedef struct FpyObj { const FpyType *ty; } FpyObj;

enum { FK_INT = 0, FK_FLOAT = 1, FK_BOOL = 2, FK_OBJ = 3 };

enum {
    FT_RAW = 1, FT_STR, FT_BYTES, FT_LIST, FT_DICT, FT_SET, FT_TUPLE, FT_INT, FT_FLOAT, FT_BOOL,
    FT_FUNC, FT_ITER, FT_CELL, FT_INSTANCE, FT_CLASS, FT_MODULE, FT_RANGE, FT_PYOBJ
};

typedef struct FpyStr FpyStr;
typedef struct FpyList FpyList;
typedef struct FpyDict FpyDict;
typedef struct FpyTuple FpyTuple;
typedef struct FpyFunc FpyFunc;
typedef struct FpyIter FpyIter;

typedef struct FpyField { const char *name; int32_t offset; int32_t kind; } FpyField;
/* Dynamic-call entry for a method/function: args[0] is self for methods. */
typedef FpyObj *(*FpyDynFn)(void *env, FpyObj **args, int64_t nargs);
typedef struct FpyMethod { const char *name; FpyDynFn fn; } FpyMethod;

struct FpyType {
    const char *name;
    const FpyType *base;
    int32_t tag;
    int32_t size;
    void (*trace)(FpyObj *);
    const int32_t *ptr_offsets;
    int32_t nptrs;
    FpyStr *(*repr)(FpyObj *);
    FpyStr *(*str)(FpyObj *);
    bool (*eq)(FpyObj *, FpyObj *);
    int64_t (*hash)(FpyObj *);
    int (*lt)(FpyObj *, FpyObj *); /* returns 1/0, or -1 if unsupported */
    const FpyField *fields;
    int32_t nfields;
    const FpyMethod *methods;
    int32_t nmethods;
    void **vtable;
    FpyDynFn ctor; /* dynamic constructor (for calling a class object) */
    void (*finalize)(FpyObj *);
};

/* ------------------------------------------------------------------ objects */

struct FpyStr {
    const FpyType *ty;
    int64_t len;    /* bytes */
    int64_t nchars; /* code points */
    int64_t hash;   /* 0 = not computed */
    char data[];    /* NUL-terminated UTF-8 */
};

typedef struct FpyBytes { const FpyType *ty; int64_t len; int64_t hash; uint8_t data[]; } FpyBytes;

typedef struct FpyRaw { const FpyType *ty; int64_t cap; fpy_word items[]; } FpyRaw;

struct FpyList {
    const FpyType *ty;
    int32_t kind;
    int64_t len;
    int64_t cap;
    fpy_word *items;
};

typedef struct FpyDictEntry { int64_t hash; fpy_word key; fpy_word value; } FpyDictEntry;

struct FpyDict {
    const FpyType *ty;
    int32_t kkind, vkind;
    int64_t len;      /* live entries */
    int64_t used;     /* entries slots used (incl. deleted) */
    int64_t ecap;     /* entries capacity */
    int64_t icap;     /* index table size (power of 2) */
    FpyDictEntry *entries;
    int64_t *index;   /* -1 empty, -2 deleted, else entry idx */
    bool is_set;
};
typedef FpyDict FpySet;

struct FpyTuple {
    const FpyType *ty;
    int64_t len;
    uint8_t *kinds;
    fpy_word items[];
};

typedef struct FpyBox { const FpyType *ty; fpy_word v; } FpyBox;

struct FpyFunc {
    const FpyType *ty;
    void *fn;      /* typed entry: R fn(void *env, args...) */
    void *env;     /* captured environment (GC object) or NULL */
    FpyDynFn dyn;  /* dynamic entry */
    const char *name;
    FpyObj *self;  /* bound receiver for bound methods, else NULL */
};

typedef struct FpyClassObj { const FpyType *ty; const FpyType *cls; } FpyClassObj;
extern const FpyType fpy_type_class;
FpyObj *fpy_class_obj(const FpyType *cls);

typedef struct FpyCell { const FpyType *ty; fpy_word v; int32_t kind; } FpyCell;

struct FpyIter {
    const FpyType *ty;
    bool (*next)(FpyIter *, fpy_word *out);
    int32_t kind;      /* element kind */
    int64_t i, n;      /* scratch */
    fpy_word a, b, c;  /* scratch (traced if objects) */
    FpyObj *o1, *o2, *o3;
};

typedef struct FpyRange { const FpyType *ty; int64_t start, stop, step; } FpyRange;

/* ------------------------------------------------------------------ builtin types */
extern const FpyType fpy_type_raw, fpy_type_str, fpy_type_bytes, fpy_type_list, fpy_type_dict,
    fpy_type_set, fpy_type_tuple, fpy_type_int, fpy_type_float, fpy_type_bool, fpy_type_func,
    fpy_type_iter, fpy_type_cell, fpy_type_range;

/* ------------------------------------------------------------------ GC */
void fpy_gc_init(void *stack_bottom);
void *fpy_alloc(const FpyType *ty, size_t size);       /* zeroed, traced per type */
void *fpy_alloc_raw(size_t bytes);                      /* untraced payload */
FpyRaw *fpy_alloc_words(int64_t n);                      /* raw word buffer */
void fpy_gc_add_root(void *start, size_t bytes);
void fpy_gc_collect(void);
void fpy_gc_mark(const void *p);
void fpy_gc_mark_word(fpy_word w, int kind);
void fpy_gc_register_stack(void *lo, void *hi);
void fpy_gc_unregister_stack(void *lo);

/* ------------------------------------------------------------------ word helpers */
static inline fpy_word fpy_wf(double d) { fpy_word w; memcpy(&w, &d, 8); return w; }
static inline double fpy_fw(fpy_word w) { double d; memcpy(&d, &w, 8); return d; }
static inline fpy_word fpy_wp(const void *p) { return (fpy_word)(uintptr_t)p; }
static inline void *fpy_pw(fpy_word w) { return (void *)(uintptr_t)w; }

/* ------------------------------------------------------------------ exceptions */
typedef struct FpyExcFrame {
    jmp_buf jb;
    struct FpyExcFrame *prev;
} FpyExcFrame;

extern _Thread_local FpyExcFrame *fpy_exc_top;
extern _Thread_local FpyObj *fpy_cur_exc;

_Noreturn void fpy_raise(FpyObj *exc);
_Noreturn void fpy_reraise(void);
_Noreturn void fpy_raise_code(int code, const char *msg);
_Noreturn void fpy_raise_fmt(int code, const char *fmt, ...);

/* Codes for runtime-raised builtin exceptions; the compiler supplies the factory. */
enum {
    FPY_EXC_EXCEPTION = 0, FPY_EXC_VALUE, FPY_EXC_TYPE, FPY_EXC_INDEX, FPY_EXC_KEY,
    FPY_EXC_ZERODIV, FPY_EXC_OVERFLOW, FPY_EXC_ATTRIBUTE, FPY_EXC_STOPITER, FPY_EXC_RUNTIME,
    FPY_EXC_ASSERTION, FPY_EXC_NOTIMPL, FPY_EXC_OS, FPY_EXC_FILENOTFOUND, FPY_EXC_RECURSION,
    FPY_EXC_MEMORY, FPY_EXC_NAME, FPY_EXC_IMPORT, FPY_EXC_UNICODE, FPY_EXC_LOOKUP,
    FPY_EXC_ARITHMETIC, FPY_EXC_SYSTEMEXIT, FPY_EXC_KEYBOARDINT, FPY_EXC_EOF, FPY_EXC_GENEXIT, FPY_EXC__COUNT
};
FpyObj *fpy_make_builtin_exc(int code, FpyStr *msg);       /* generated by compiler */
const FpyType *fpy_builtin_exc_type(int code);              /* generated by compiler */
FpyStr *fpy_exc_message(FpyObj *exc);                       /* generated by compiler */

static inline bool fpy_isinstance_t(const FpyObj *o, const FpyType *t) {
    if (!o) return false;
    for (const FpyType *x = o->ty; x; x = x->base)
        if (x == t) return true;
    return false;
}

#define FPY_TRY(frame) ((frame).prev = fpy_exc_top, fpy_exc_top = &(frame), setjmp((frame).jb) == 0)
#define FPY_POP(frame) (fpy_exc_top = (frame).prev)

/* ------------------------------------------------------------------ ints/floats */
_Noreturn void fpy_overflow(void);
_Noreturn void fpy_zerodiv(void);

static inline fpy_int fpy_add(fpy_int a, fpy_int b) { fpy_int r; if (__builtin_add_overflow(a, b, &r)) fpy_overflow(); return r; }
static inline fpy_int fpy_sub(fpy_int a, fpy_int b) { fpy_int r; if (__builtin_sub_overflow(a, b, &r)) fpy_overflow(); return r; }
static inline fpy_int fpy_mul(fpy_int a, fpy_int b) { fpy_int r; if (__builtin_mul_overflow(a, b, &r)) fpy_overflow(); return r; }
static inline fpy_int fpy_neg(fpy_int a) { if (a == INT64_MIN) fpy_overflow(); return -a; }
static inline fpy_int fpy_floordiv(fpy_int a, fpy_int b) {
    if (__builtin_expect(b == 0, 0)) fpy_zerodiv();
    if (a == INT64_MIN && b == -1) fpy_overflow();
    fpy_int q = a / b;
    if ((a % b != 0) && ((a < 0) != (b < 0))) q--;
    return q;
}
static inline fpy_int fpy_mod(fpy_int a, fpy_int b) {
    if (__builtin_expect(b == 0, 0)) fpy_zerodiv();
    if (b == -1) return 0;
    fpy_int r = a % b;
    if (r != 0 && ((r < 0) != (b < 0))) r += b;
    return r;
}
static inline double fpy_truediv(double a, double b) {
    if (__builtin_expect(b == 0.0, 0)) fpy_zerodiv();
    return a / b;
}
static inline double fpy_ffloordiv(double a, double b) {
    if (b == 0.0) fpy_zerodiv();
    return floor(a / b);
}
static inline double fpy_fmod(double a, double b) {
    if (b == 0.0) fpy_zerodiv();
    double r = fmod(a, b);
    if (r != 0.0 && ((r < 0) != (b < 0))) r += b;
    return r;
}
fpy_int fpy_ipow(fpy_int a, fpy_int b);
static inline fpy_int fpy_lshift(fpy_int a, fpy_int b) {
    if (b < 0) fpy_raise_code(FPY_EXC_VALUE, "negative shift count");
    if (b >= 64 || (a != 0 && (a > (INT64_MAX >> b) || a < (INT64_MIN >> b)))) fpy_overflow();
    return (fpy_int)((uint64_t)a << b);
}
static inline fpy_int fpy_rshift(fpy_int a, fpy_int b) {
    if (b < 0) fpy_raise_code(FPY_EXC_VALUE, "negative shift count");
    if (b >= 64) return a < 0 ? -1 : 0;
    return a >> b;
}
static inline fpy_int fpy_iabs(fpy_int a) { return a < 0 ? fpy_neg(a) : a; }

/* ------------------------------------------------------------------ strings */
FpyStr *fpy_str_new(const char *data, int64_t len);
FpyStr *fpy_str_lit(const char *data, int64_t len, int64_t nchars); /* for literals */
FpyStr *fpy_str_from_cstr(const char *s);
FpyStr *fpy_str_concat(FpyStr *a, FpyStr *b);
FpyStr *fpy_str_repeat(FpyStr *a, fpy_int n);
bool fpy_str_eq(FpyStr *a, FpyStr *b);
int fpy_str_cmp(FpyStr *a, FpyStr *b);
int64_t fpy_str_hash(FpyStr *s);
FpyStr *fpy_str_getitem(FpyStr *s, fpy_int i);
FpyStr *fpy_str_slice(FpyStr *s, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi, fpy_int step);
bool fpy_str_contains(FpyStr *hay, FpyStr *needle);
FpyStr *fpy_int_to_str(fpy_int v);
FpyStr *fpy_float_to_str(double v);
FpyStr *fpy_bool_to_str(bool b);
FpyStr *fpy_str_repr(FpyStr *s);
FpyStr *fpy_ascii_escape(FpyStr *s);
FpyStr *fpy_str_char(uint32_t cp);
fpy_int fpy_str_to_int(FpyStr *s, fpy_int base);
double fpy_str_to_float(FpyStr *s);
static inline fpy_int fpy_str_len(FpyStr *s) { return s->nchars; }

/* string methods */
FpyStr *fpy_str_upper(FpyStr *s);
FpyStr *fpy_str_lower(FpyStr *s);
FpyStr *fpy_str_strip(FpyStr *s, FpyStr *chars);
FpyStr *fpy_str_lstrip(FpyStr *s, FpyStr *chars);
FpyStr *fpy_str_rstrip(FpyStr *s, FpyStr *chars);
FpyList *fpy_str_split(FpyStr *s, FpyStr *sep, fpy_int maxsplit);
FpyList *fpy_str_rsplit(FpyStr *s, FpyStr *sep, fpy_int maxsplit);
FpyList *fpy_str_splitlines(FpyStr *s, bool keepends);
FpyStr *fpy_str_join(FpyStr *sep, FpyList *items);
FpyStr *fpy_str_replace(FpyStr *s, FpyStr *old, FpyStr *new_, fpy_int count);
bool fpy_str_startswith(FpyStr *s, FpyStr *p);
bool fpy_str_endswith(FpyStr *s, FpyStr *p);
fpy_int fpy_str_find(FpyStr *s, FpyStr *sub);
fpy_int fpy_str_rfind(FpyStr *s, FpyStr *sub);
fpy_int fpy_str_index(FpyStr *s, FpyStr *sub);
fpy_int fpy_str_count(FpyStr *s, FpyStr *sub);
bool fpy_str_isdigit(FpyStr *s);
bool fpy_str_isalpha(FpyStr *s);
bool fpy_str_isalnum(FpyStr *s);
bool fpy_str_isspace(FpyStr *s);
bool fpy_str_isupper(FpyStr *s);
bool fpy_str_islower(FpyStr *s);
bool fpy_str_isidentifier(FpyStr *s);
bool fpy_str_istitle(FpyStr *s);
bool fpy_str_isascii(FpyStr *s);
bool fpy_str_isprintable(FpyStr *s);
FpyStr *fpy_str_capitalize(FpyStr *s);
FpyStr *fpy_str_title(FpyStr *s);
FpyStr *fpy_str_swapcase(FpyStr *s);
FpyStr *fpy_str_center(FpyStr *s, fpy_int w, FpyStr *fill);
FpyStr *fpy_str_ljust(FpyStr *s, fpy_int w, FpyStr *fill);
FpyStr *fpy_str_rjust(FpyStr *s, fpy_int w, FpyStr *fill);
FpyStr *fpy_str_zfill(FpyStr *s, fpy_int w);
FpyStr *fpy_str_expandtabs(FpyStr *s, fpy_int tabsize);
FpyStr *fpy_str_removeprefix(FpyStr *s, FpyStr *p);
FpyStr *fpy_str_removesuffix(FpyStr *s, FpyStr *p);
FpyTuple *fpy_str_partition(FpyStr *s, FpyStr *sep);
FpyTuple *fpy_str_rpartition(FpyStr *s, FpyStr *sep);
fpy_int fpy_str_ord(FpyStr *s);
FpyStr *fpy_chr(fpy_int cp);
FpyBytes *fpy_str_encode(FpyStr *s);
FpyIter *fpy_str_iter(FpyStr *s);

/* string builder (for f-strings and joins) */
typedef struct FpySB { char *buf; int64_t len, cap; int64_t nchars; } FpySB;
void fpy_sb_init(FpySB *sb);
void fpy_sb_add(FpySB *sb, const char *s, int64_t n);
void fpy_sb_add_str(FpySB *sb, FpyStr *s);
void fpy_sb_add_cstr(FpySB *sb, const char *s);
FpyStr *fpy_sb_finish(FpySB *sb);
FpyStr *fpy_format_int(fpy_int v, FpyStr *spec);
FpyStr *fpy_format_float(double v, FpyStr *spec);
FpyStr *fpy_format_str(FpyStr *v, FpyStr *spec);

/* bytes */
FpyBytes *fpy_bytes_new(const uint8_t *data, int64_t len);
FpyStr *fpy_bytes_decode(FpyBytes *b);
FpyStr *fpy_bytes_repr(FpyBytes *b);
bool fpy_bytes_eq(FpyBytes *a, FpyBytes *b);
fpy_int fpy_bytes_getitem(FpyBytes *b, fpy_int i);
FpyBytes *fpy_bytes_concat(FpyBytes *a, FpyBytes *b);
FpyIter *fpy_bytes_iter(FpyBytes *b);
FpyBytes *fpy_bytes_repeat(FpyBytes *b, fpy_int n);
bool fpy_bytes_contains_byte(FpyBytes *b, fpy_int v);
bool fpy_bytes_contains(FpyBytes *b, FpyBytes *sub);
FpyBytes *fpy_bytes_slice(FpyBytes *b, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi, fpy_int step);

/* ------------------------------------------------------------------ lists */
FpyList *fpy_list_new(int kind, int64_t cap);
FpyList *fpy_list_from(int kind, int64_t n, const fpy_word *items);
void fpy_list_grow(FpyList *l, int64_t need);
static inline void fpy_list_append(FpyList *l, fpy_word v) {
    if (__builtin_expect(l->len == l->cap, 0)) fpy_list_grow(l, l->len + 1);
    l->items[l->len++] = v;
}
_Noreturn void fpy_index_error(const char *what, fpy_int i, fpy_int len);
static inline int64_t fpy_norm_index(fpy_int i, int64_t len, const char *what) {
    if (i < 0) i += len;
    if (__builtin_expect((uint64_t)i >= (uint64_t)len, 0)) fpy_index_error(what, i, len);
    return i;
}
static inline fpy_word fpy_list_get(FpyList *l, fpy_int i) { return l->items[fpy_norm_index(i, l->len, "list")]; }
static inline void fpy_list_set(FpyList *l, fpy_int i, fpy_word v) { l->items[fpy_norm_index(i, l->len, "list assignment")] = v; }
fpy_word fpy_list_pop(FpyList *l, fpy_int i);
void fpy_list_insert(FpyList *l, fpy_int i, fpy_word v);
void fpy_list_extend(FpyList *l, FpyList *other);
void fpy_list_extend_iter(FpyList *l, FpyIter *it);
FpyList *fpy_list_slice(FpyList *l, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi, fpy_int step);
void fpy_list_set_slice(FpyList *l, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi, FpyList *v);
void fpy_list_del(FpyList *l, fpy_int i);
void fpy_list_del_slice(FpyList *l, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi);
FpyList *fpy_list_concat(FpyList *a, FpyList *b);
FpyList *fpy_list_repeat(FpyList *a, fpy_int n);
bool fpy_list_contains(FpyList *l, fpy_word v);
bool fpy_iter_contains(FpyIter *it, int kind, fpy_word v);
fpy_int fpy_list_index(FpyList *l, fpy_word v);
fpy_int fpy_list_count(FpyList *l, fpy_word v);
void fpy_list_remove(FpyList *l, fpy_word v);
void fpy_list_reverse(FpyList *l);
void fpy_list_sort(FpyList *l, bool reverse);
void fpy_list_sort_by(FpyList *l, FpyList *keys, bool reverse);
void fpy_list_clear(FpyList *l);
FpyList *fpy_list_copy(FpyList *l);
FpyIter *fpy_list_iter(FpyList *l);
FpyList *fpy_list_from_iter(int kind, FpyIter *it);

/* ------------------------------------------------------------------ dicts & sets */
FpyDict *fpy_dict_new(int kkind, int vkind);
bool fpy_dict_lookup(FpyDict *d, fpy_word key, fpy_word *out);
fpy_word fpy_dict_get(FpyDict *d, fpy_word key);           /* raises KeyError */
void fpy_dict_set(FpyDict *d, fpy_word key, fpy_word value);
bool fpy_dict_contains(FpyDict *d, fpy_word key);
fpy_word *fpy_dict_slot(FpyDict *d, fpy_word key);
bool fpy_dict_del(FpyDict *d, fpy_word key, bool raise);
fpy_word fpy_dict_pop(FpyDict *d, fpy_word key, bool has_default, fpy_word dflt);
fpy_word fpy_dict_setdefault(FpyDict *d, fpy_word key, fpy_word dflt);
void fpy_dict_update(FpyDict *d, FpyDict *other);
void fpy_dict_clear(FpyDict *d);
FpyDict *fpy_dict_copy(FpyDict *d);
FpyList *fpy_dict_keys(FpyDict *d);
FpyList *fpy_dict_values(FpyDict *d);
FpyList *fpy_dict_items(FpyDict *d);
FpyTuple *fpy_dict_popitem(FpyDict *d);
FpyIter *fpy_dict_iter_keys(FpyDict *d);
FpyIter *fpy_dict_iter_values(FpyDict *d);
FpyIter *fpy_dict_iter_items(FpyDict *d);
bool fpy_dict_eq(FpyDict *a, FpyDict *b);
/* Iterate live entries: for (int64_t i = 0; fpy_dict_next(d, &i, &e); ) */
#define FPY_DELETED_HASH INT64_MIN
static inline bool fpy_dict_next(FpyDict *d, int64_t *pos, FpyDictEntry **e) {
    while (*pos < d->used) {
        FpyDictEntry *x = &d->entries[(*pos)++];
        if (x->hash != FPY_DELETED_HASH) { *e = x; return true; }
    }
    return false;
}

FpySet *fpy_set_new(int kind);
void fpy_set_add(FpySet *s, fpy_word v);
bool fpy_set_contains(FpySet *s, fpy_word v);
void fpy_set_discard(FpySet *s, fpy_word v);
void fpy_set_remove(FpySet *s, fpy_word v);
fpy_word fpy_set_pop(FpySet *s);
FpySet *fpy_set_union(FpySet *a, FpySet *b);
FpySet *fpy_set_intersection(FpySet *a, FpySet *b);
FpySet *fpy_set_difference(FpySet *a, FpySet *b);
FpySet *fpy_set_symdiff(FpySet *a, FpySet *b);
bool fpy_set_issubset(FpySet *a, FpySet *b);
FpyIter *fpy_set_iter(FpySet *s);
FpySet *fpy_set_from_iter(int kind, FpyIter *it);

/* ------------------------------------------------------------------ tuples, boxes, cells */
FpyTuple *fpy_tuple_new(int64_t n, const uint8_t *kinds);
FpyTuple *fpy_tuple_uniform(int64_t n, int kind);
FpyTuple *fpy_tuple_from_list(FpyList *l);
FpyTuple *fpy_tuple_from_iter(int kind, FpyIter *it);
FpyTuple *fpy_tuple_retype(FpyTuple *t, int kind);
fpy_word fpy_tuple_get(FpyTuple *t, fpy_int i);
FpyTuple *fpy_tuple_slice(FpyTuple *t, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi, fpy_int step);
FpyTuple *fpy_tuple_concat(FpyTuple *a, FpyTuple *b);
bool fpy_tuple_contains(FpyTuple *t, fpy_word v, int kind);
bool fpy_tuple_eq(FpyTuple *a, FpyTuple *b);
int fpy_tuple_cmp(FpyTuple *a, FpyTuple *b);
FpyObj *fpy_box_int(fpy_int v);
FpyObj *fpy_box_float(double v);
FpyObj *fpy_box_bool(bool v);
FpyCell *fpy_cell_new(int kind);
FpyObj *fpy_box_word(fpy_word w, int kind);
fpy_word fpy_unbox_word(FpyObj *o, int kind, const char *tyname);
FpyIter *fpy_dyn_iter_kind(FpyObj *o, int kind);
static inline fpy_int fpy_unbox_int(FpyObj *o) { return (fpy_int)((FpyBox *)o)->v; }
static inline double fpy_unbox_float(FpyObj *o) { return fpy_fw(((FpyBox *)o)->v); }

/* ------------------------------------------------------------------ functions & iterators */
FpyFunc *fpy_func_new(void *fn, void *env, FpyDynFn dyn, const char *name);
FpyIter *fpy_iter_new(bool (*next)(FpyIter *, fpy_word *), int kind);
FpyIter *fpy_range_iter(fpy_int start, fpy_int stop, fpy_int step);
FpyIter *fpy_tuple_iter(FpyTuple *t);
FpyIter *fpy_enumerate(FpyIter *it, fpy_int start);
FpyIter *fpy_zip(FpyIter *a, FpyIter *b);
FpyIter *fpy_iter_from_protocol(FpyObj *o, int kind);
FpyIter *fpy_reversed_list(FpyList *l);
_Noreturn void fpy_raise_stop_iteration(void);

/* ------------------------------------------------------------------ generators (stackful coroutines) */
typedef struct FpyGen FpyGen;
typedef void (*FpyGenBody)(FpyGen *g);
FpyIter *fpy_gen_new(FpyGenBody body, void *frame, int kind);
void fpy_gen_yield(FpyGen *g, fpy_word v);
void *fpy_gen_frame(FpyGen *g);
int fpy_gen_step(FpyIter *it, fpy_word sent, FpyObj *thrown, fpy_word *out);
fpy_word fpy_gen_take_sent(FpyGen *g);
fpy_word fpy_gen_yield_recv(FpyGen *g, fpy_word v);
fpy_word fpy_gen_send(FpyIter *it, FpyObj *v);
fpy_word fpy_gen_throw(FpyIter *it, FpyObj *exc);
void fpy_gen_close(FpyIter *it);
FpyObj *fpy_gen_take_thrown(FpyGen *g);
void fpy_gen_set_result(FpyGen *g, fpy_word r);
fpy_word fpy_gen_result(FpyIter *it);
bool fpy_gen_done(FpyIter *it);

/* ------------------------------------------------------------------ repr / str / eq / hash on words */
FpyStr *fpy_repr_word(fpy_word w, int kind);
FpyStr *fpy_str_word(fpy_word w, int kind);
bool fpy_eq_word(fpy_word a, fpy_word b, int kind);
int64_t fpy_hash_word(fpy_word w, int kind);
int fpy_cmp_word(fpy_word a, fpy_word b, int kind); /* -1/0/1, raises TypeError if unorderable */
FpyStr *fpy_obj_repr(FpyObj *o);
FpyStr *fpy_obj_str(FpyObj *o);
bool fpy_obj_eq(FpyObj *a, FpyObj *b);
int64_t fpy_obj_hash(FpyObj *o);
int fpy_obj_cmp(FpyObj *a, FpyObj *b);
FpyStr *fpy_default_repr(FpyObj *o);

/* ------------------------------------------------------------------ I/O & process */
void fpy_print_str(FpyStr *s, FpyStr *sep_or_null);
void fpy_write(FpyStr *s);
void fpy_write_err(FpyStr *s);
void fpy_flush(void);
FpyStr *fpy_input(FpyStr *prompt);
_Noreturn void fpy_exit(fpy_int code);
extern FpyList *fpy_argv;
FpyStr *fpy_getenv(FpyStr *name, FpyStr *dflt);
FpyStr *fpy_read_file(FpyStr *path);
void fpy_write_file(FpyStr *path, FpyStr *data, bool append);
bool fpy_path_exists(FpyStr *path);
double fpy_time_now(void);
FpyList *fpy_get_argv(void);
FpyStr *fpy_platform(void);
FpyDict *fpy_environ(void);
FpyStr *fpy_getcwd(void);
fpy_int fpy_getpid(void);
FpyList *fpy_listdir(FpyStr *p);
void fpy_mkdir(FpyStr *p);
void fpy_remove(FpyStr *p);
void fpy_rmdir(FpyStr *p);
void fpy_chdir(FpyStr *p);
fpy_int fpy_cpu_count(void);
bool fpy_path_isfile(FpyStr *p);
bool fpy_path_isdir(FpyStr *p);
double fpy_perf_counter(void);
fpy_int fpy_time_ns(void);
void fpy_sleep(double secs);
fpy_int fpy_random_bits(void);
void fpy_random_seed(fpy_int seed);
double fpy_random_float(void);
fpy_int fpy_random_range(fpy_int lo, fpy_int hi);
FpyStr *fpy_float_hex(double v);
double fpy_round(double v, fpy_int nd);
fpy_int fpy_round_int(double v);

/* ------------------------------------------------------------------ dynamic (Any) operations */
FpyObj *fpy_dyn_binop(int op, FpyObj *a, FpyObj *b);
FpyObj *fpy_dyn_neg(FpyObj *a);
bool fpy_dyn_truthy(FpyObj *a);
int fpy_dyn_compare(int op, FpyObj *a, FpyObj *b);
FpyObj *fpy_dyn_getattr(FpyObj *o, const char *name);
void fpy_dyn_setattr(FpyObj *o, const char *name, FpyObj *v);
FpyObj *fpy_dyn_getattr_default(FpyObj *o, const char *name, FpyObj *dflt);
bool fpy_dyn_hasattr(FpyObj *o, const char *name);
FpyObj *fpy_dyn_next(FpyObj *it);
FpyObj *fpy_dyn_next_default(FpyObj *it, FpyObj *dflt);
FpyObj *fpy_dyn_getitem(FpyObj *o, FpyObj *k);
void fpy_dyn_setitem(FpyObj *o, FpyObj *k, FpyObj *v);
FpyObj *fpy_dyn_call(FpyObj *f, FpyObj **args, int64_t nargs);
FpyObj *fpy_dyn_call_method(FpyObj *o, const char *name, FpyObj **args, int64_t nargs);
fpy_int fpy_dyn_len(FpyObj *o);
FpyIter *fpy_dyn_iter(FpyObj *o);
bool fpy_dyn_contains(FpyObj *container, FpyObj *item);
const char *fpy_type_name(FpyObj *o);
fpy_int fpy_dyn_to_int(FpyObj *o);
double fpy_dyn_to_float(FpyObj *o);
fpy_int fpy_float_to_int(double v);
fpy_int fpy_dyn_int(FpyObj *o);
double fpy_dyn_float(FpyObj *o);
/* Python bridge hooks (NULL unless built with the CPython fallback) */
struct FpyPyHooks {
    FpyObj *(*getattr)(FpyObj *, const char *);
    void (*setattr)(FpyObj *, const char *, FpyObj *);
    FpyObj *(*call)(FpyObj *, FpyObj **, int64_t, const char **, FpyObj **, int64_t);
    FpyObj *(*call_method)(FpyObj *, const char *, FpyObj **, int64_t, const char **, FpyObj **, int64_t);
    FpyObj *(*getitem)(FpyObj *, FpyObj *);
    void (*setitem)(FpyObj *, FpyObj *, FpyObj *);
    fpy_int (*len)(FpyObj *);
    bool (*truthy)(FpyObj *);
    bool (*contains)(FpyObj *, FpyObj *);
    FpyIter *(*iter)(FpyObj *);
    FpyObj *(*binop)(int, FpyObj *, FpyObj *);
    int (*compare)(int, FpyObj *, FpyObj *);
    bool (*exc_matches_native)(FpyObj *, const FpyType *);
    bool (*exc_matches_dyn)(FpyObj *, FpyObj *);
    const char *(*type_name)(FpyObj *);
    void (*mark_roots)(void);
    void (*scan_threads)(void);
    void *(*thread_bottom)(void);
    FpyObj *(*exc_instance)(FpyObj *);
    void (*print_exc)(FpyObj *);
    int (*exit_code)(FpyObj *, bool *is_exit);
    bool (*isinstance)(FpyObj *, FpyObj *);
};
FpyObj *fpy_exc_instance(FpyObj *o);
FpyObj *fpy_exc_type_of(FpyObj *e);
bool fpy_dyn_isinstance(FpyObj *o, FpyObj *cls);
extern struct FpyPyHooks *fpy_py;
static inline bool fpy_is_py(const FpyObj *o) { return o && o->ty->tag == FT_PYOBJ && fpy_py; }
FpyObj *fpy_dyn_call_kw(FpyObj *f, FpyObj **args, int64_t nargs, const char **kwn, FpyObj **kwv, int64_t nk);
FpyObj *fpy_dyn_call_method_kw(FpyObj *o, const char *name, FpyObj **args, int64_t nargs, const char **kwn, FpyObj **kwv, int64_t nk);
bool fpy_exc_match(FpyObj *e, const FpyType *t);
bool fpy_exc_match_dyn(FpyObj *e, FpyObj *cls);
FpyObj *fpy_exc_as(FpyObj *e, const FpyType *t);
FpyList *fpy_retype_list(FpyList *l, int kind);
FpyDict *fpy_retype_dict(FpyDict *d, int kk, int vk);
extern FpyObj *fpy_missing;
FpyObj *fpy_check_cast(FpyObj *o, const FpyType *t, const char *tyname);

#endif
