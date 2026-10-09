/* fpython runtime — list, dict, set, tuple. */
#include "fpy.h"

extern int64_t fpy_slice_len(int64_t start, int64_t stop, int64_t step);
extern void fpy_slice_indices(int64_t len, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi, fpy_int step,
                              int64_t *start, int64_t *stop);

/* ================================================================== list */
static void list_trace(FpyObj *o) {
    FpyList *l = (FpyList *)o;
    if (l->items) fpy_gc_mark(l->items);
    if (l->kind == FK_OBJ)
        for (int64_t i = 0; i < l->len; i++)
            if (l->items[i]) fpy_gc_mark(fpy_pw(l->items[i]));
}

const FpyType fpy_type_list = {.name = "list", .tag = FT_LIST, .trace = list_trace};

FpyList *fpy_list_new(int kind, int64_t cap) {
    FpyList *l = fpy_alloc(&fpy_type_list, sizeof(FpyList));
    l->kind = kind;
    if (cap < 4) cap = 4;
    l->items = fpy_alloc_words(cap)->items;
    l->cap = cap;
    return l;
}

FpyList *fpy_list_from(int kind, int64_t n, const fpy_word *items) {
    FpyList *l = fpy_list_new(kind, n);
    if (n) memcpy(l->items, items, (size_t)n * sizeof(fpy_word));
    l->len = n;
    return l;
}

void fpy_list_grow(FpyList *l, int64_t need) {
    int64_t cap = l->cap ? l->cap : 4;
    while (cap < need) cap = cap + (cap >> 1) + 4;
    fpy_word *items = fpy_alloc_words(cap)->items;
    if (l->len) memcpy(items, l->items, (size_t)l->len * sizeof(fpy_word));
    l->items = items;
    l->cap = cap;
}

_Noreturn void fpy_index_error(const char *what, fpy_int i, fpy_int len) {
    (void)i;
    (void)len;
    fpy_raise_fmt(FPY_EXC_INDEX, "%s index out of range", what);
}

fpy_word fpy_list_pop(FpyList *l, fpy_int i) {
    if (l->len == 0) fpy_raise_code(FPY_EXC_INDEX, "pop from empty list");
    int64_t k = fpy_norm_index(i, l->len, "pop");
    fpy_word v = l->items[k];
    memmove(&l->items[k], &l->items[k + 1], (size_t)(l->len - k - 1) * sizeof(fpy_word));
    l->len--;
    return v;
}

void fpy_list_insert(FpyList *l, fpy_int i, fpy_word v) {
    if (i < 0) { i += l->len; if (i < 0) i = 0; }
    if (i > l->len) i = l->len;
    if (l->len == l->cap) fpy_list_grow(l, l->len + 1);
    memmove(&l->items[i + 1], &l->items[i], (size_t)(l->len - i) * sizeof(fpy_word));
    l->items[i] = v;
    l->len++;
}

void fpy_list_extend(FpyList *l, FpyList *o) {
    int64_t n = o->len;
    if (l->len + n > l->cap) fpy_list_grow(l, l->len + n);
    memmove(&l->items[l->len], o->items, (size_t)n * sizeof(fpy_word));
    l->len += n;
}

void fpy_list_extend_iter(FpyList *l, FpyIter *it) {
    fpy_word w;
    while (it->next(it, &w)) fpy_list_append(l, w);
}

FpyList *fpy_list_from_iter(int kind, FpyIter *it) {
    FpyList *l = fpy_list_new(kind, 8);
    fpy_list_extend_iter(l, it);
    return l;
}

FpyList *fpy_list_slice(FpyList *l, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi, fpy_int step) {
    int64_t start, stop;
    fpy_slice_indices(l->len, has_lo, lo, has_hi, hi, step, &start, &stop);
    int64_t n = fpy_slice_len(start, stop, step);
    FpyList *r = fpy_list_new(l->kind, n);
    if (step == 1) {
        if (n > 0) memcpy(r->items, &l->items[start], (size_t)n * sizeof(fpy_word));
    } else {
        for (int64_t k = 0, i = start; k < n; k++, i += step) r->items[k] = l->items[i];
    }
    r->len = n > 0 ? n : 0;
    return r;
}

void fpy_list_set_slice(FpyList *l, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi, FpyList *v) {
    int64_t start, stop;
    fpy_slice_indices(l->len, has_lo, lo, has_hi, hi, 1, &start, &stop);
    if (stop < start) stop = start;
    FpyList *src = v == l ? fpy_list_copy(v) : v;
    int64_t n = src->len, removed = stop - start;
    int64_t newlen = l->len - removed + n;
    if (newlen > l->cap) fpy_list_grow(l, newlen);
    memmove(&l->items[start + n], &l->items[stop], (size_t)(l->len - stop) * sizeof(fpy_word));
    memcpy(&l->items[start], src->items, (size_t)n * sizeof(fpy_word));
    l->len = newlen;
}

void fpy_list_del(FpyList *l, fpy_int i) {
    int64_t k = fpy_norm_index(i, l->len, "list assignment");
    memmove(&l->items[k], &l->items[k + 1], (size_t)(l->len - k - 1) * sizeof(fpy_word));
    l->len--;
}

void fpy_list_del_slice(FpyList *l, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi) {
    FpyList empty = {.ty = &fpy_type_list, .kind = l->kind, .len = 0, .cap = 0, .items = NULL};
    fpy_list_set_slice(l, has_lo, lo, has_hi, hi, &empty);
}

FpyList *fpy_list_concat(FpyList *a, FpyList *b) {
    FpyList *r = fpy_list_new(a->kind, a->len + b->len);
    memcpy(r->items, a->items, (size_t)a->len * sizeof(fpy_word));
    memcpy(r->items + a->len, b->items, (size_t)b->len * sizeof(fpy_word));
    r->len = a->len + b->len;
    return r;
}

FpyList *fpy_list_repeat(FpyList *a, fpy_int n) {
    if (n < 0) n = 0;
    FpyList *r = fpy_list_new(a->kind, a->len * n);
    for (fpy_int i = 0; i < n; i++) memcpy(r->items + i * a->len, a->items, (size_t)a->len * sizeof(fpy_word));
    r->len = a->len * n;
    return r;
}

/* `v in iterator`: consumes items up to and including the first match, like CPython. */
bool fpy_iter_contains(FpyIter *it, int kind, fpy_word v) {
    fpy_word x;
    while (it->next(it, &x))
        if (kind != FK_OBJ && kind != FK_FLOAT ? x == v : fpy_eq_word(x, v, kind)) return true;
    return false;
}

bool fpy_list_contains(FpyList *l, fpy_word v) {
    if (l->kind != FK_OBJ && l->kind != FK_FLOAT) {
        for (int64_t i = 0; i < l->len; i++)
            if (l->items[i] == v) return true;
        return false;
    }
    for (int64_t i = 0; i < l->len; i++)
        if (fpy_eq_word(l->items[i], v, l->kind)) return true;
    return false;
}

fpy_int fpy_list_index(FpyList *l, fpy_word v) {
    for (int64_t i = 0; i < l->len; i++)
        if (fpy_eq_word(l->items[i], v, l->kind)) return i;
    fpy_raise_code(FPY_EXC_VALUE, "list.index(x): x not in list");
}

fpy_int fpy_list_count(FpyList *l, fpy_word v) {
    fpy_int n = 0;
    for (int64_t i = 0; i < l->len; i++) n += fpy_eq_word(l->items[i], v, l->kind);
    return n;
}

void fpy_list_remove(FpyList *l, fpy_word v) {
    for (int64_t i = 0; i < l->len; i++)
        if (fpy_eq_word(l->items[i], v, l->kind)) { fpy_list_del(l, i); return; }
    fpy_raise_code(FPY_EXC_VALUE, "list.remove(x): x not in list");
}

void fpy_list_reverse(FpyList *l) {
    for (int64_t i = 0, j = l->len - 1; i < j; i++, j--) {
        fpy_word t = l->items[i];
        l->items[i] = l->items[j];
        l->items[j] = t;
    }
}

void fpy_list_clear(FpyList *l) { l->len = 0; }

FpyList *fpy_list_copy(FpyList *l) { return fpy_list_from(l->kind, l->len, l->items); }

/* Stable merge sort on words; cmp via kind, with optional parallel key array. */
static int sort_kind;
static bool sort_reverse;

static inline int cmp_items(fpy_word a, fpy_word b) {
    int c;
    switch (sort_kind) {
    case FK_INT:
    case FK_BOOL: c = (fpy_int)a < (fpy_int)b ? -1 : ((fpy_int)a > (fpy_int)b); break;
    case FK_FLOAT: { double x = fpy_fw(a), y = fpy_fw(b); c = x < y ? -1 : (x > y); break; }
    default: c = fpy_cmp_word(a, b, sort_kind);
    }
    return sort_reverse ? -c : c;
}

static void merge_sort(fpy_word *a, fpy_word *ka, fpy_word *tmp, fpy_word *ktmp, int64_t n) {
    if (n < 2) return;
    if (n <= 16) {
        for (int64_t i = 1; i < n; i++) {
            fpy_word x = a[i], kx = ka ? ka[i] : 0;
            int64_t j = i - 1;
            while (j >= 0 && cmp_items(ka ? ka[j] : a[j], ka ? kx : x) > 0) {
                a[j + 1] = a[j];
                if (ka) ka[j + 1] = ka[j];
                j--;
            }
            a[j + 1] = x;
            if (ka) ka[j + 1] = kx;
        }
        return;
    }
    int64_t m = n / 2;
    merge_sort(a, ka, tmp, ktmp, m);
    merge_sort(a + m, ka ? ka + m : NULL, tmp, ktmp, n - m);
    if (cmp_items(ka ? ka[m - 1] : a[m - 1], ka ? ka[m] : a[m]) <= 0) return;
    memcpy(tmp, a, (size_t)m * sizeof(fpy_word));
    if (ka) memcpy(ktmp, ka, (size_t)m * sizeof(fpy_word));
    int64_t i = 0, j = m, k = 0;
    while (i < m && j < n) {
        if (cmp_items(ka ? ka[j] : a[j], ka ? ktmp[i] : tmp[i]) < 0) {
            a[k] = a[j];
            if (ka) ka[k] = ka[j];
            j++;
        } else {
            a[k] = tmp[i];
            if (ka) ka[k] = ktmp[i];
            i++;
        }
        k++;
    }
    while (i < m) {
        a[k] = tmp[i];
        if (ka) ka[k] = ktmp[i];
        i++;
        k++;
    }
}

void fpy_list_sort(FpyList *l, bool reverse) {
    int ok = sort_kind, orv = sort_reverse;
    sort_kind = l->kind;
    sort_reverse = reverse;
    fpy_word *tmp = fpy_alloc_words(l->len / 2 + 1)->items;
    merge_sort(l->items, NULL, tmp, NULL, l->len);
    sort_kind = ok;
    sort_reverse = orv;
}

void fpy_list_sort_by(FpyList *l, FpyList *keys, bool reverse) {
    int ok = sort_kind, orv = sort_reverse;
    sort_kind = keys->kind;
    sort_reverse = reverse;
    fpy_word *tmp = fpy_alloc_words(l->len / 2 + 1)->items;
    fpy_word *ktmp = fpy_alloc_words(l->len / 2 + 1)->items;
    merge_sort(l->items, keys->items, tmp, ktmp, l->len);
    sort_kind = ok;
    sort_reverse = orv;
}

static bool list_iter_next(FpyIter *it, fpy_word *out) {
    FpyList *l = (FpyList *)it->o1;
    if (it->i >= l->len) return false;
    *out = l->items[it->i++];
    return true;
}

FpyIter *fpy_list_iter(FpyList *l) {
    FpyIter *it = fpy_iter_new(list_iter_next, l->kind);
    it->o1 = (FpyObj *)l;
    return it;
}

static bool list_rev_next(FpyIter *it, fpy_word *out) {
    FpyList *l = (FpyList *)it->o1;
    if (it->i < 0 || it->i >= l->len) return false;
    *out = l->items[it->i--];
    return true;
}

FpyIter *fpy_reversed_list(FpyList *l) {
    FpyIter *it = fpy_iter_new(list_rev_next, l->kind);
    it->o1 = (FpyObj *)l;
    it->i = l->len - 1;
    return it;
}

/* ================================================================== dict */
static void dict_trace(FpyObj *o) {
    FpyDict *d = (FpyDict *)o;
    if (d->entries) fpy_gc_mark(d->entries);
    if (d->index) fpy_gc_mark(d->index);
    bool kobj = d->kkind == FK_OBJ, vobj = d->vkind == FK_OBJ;
    if (!kobj && !vobj) return;
    for (int64_t i = 0; i < d->used; i++) {
        FpyDictEntry *e = &d->entries[i];
        if (e->hash == FPY_DELETED_HASH) continue;
        if (kobj && e->key) fpy_gc_mark(fpy_pw(e->key));
        if (vobj && e->value) fpy_gc_mark(fpy_pw(e->value));
    }
}

const FpyType fpy_type_dict = {.name = "dict", .tag = FT_DICT, .trace = dict_trace};
const FpyType fpy_type_set = {.name = "set", .tag = FT_SET, .trace = dict_trace};

static inline int64_t hash_key(fpy_word k, int kind) {
    int64_t h;
    if (kind == FK_OBJ && k && ((FpyObj *)fpy_pw(k))->ty == &fpy_type_str) {
        h = fpy_str_hash((FpyStr *)fpy_pw(k));
        return h == FPY_DELETED_HASH ? 7 : h;
    }
    if (kind == FK_INT || kind == FK_BOOL) {
        uint64_t x = k;
        x ^= x >> 33;
        x *= 0xff51afd7ed558ccdull;
        x ^= x >> 33;
        h = (int64_t)x;
    } else {
        h = fpy_hash_word(k, kind);
    }
    if (h == FPY_DELETED_HASH) h = 7;
    return h;
}

static inline bool key_eq(fpy_word a, fpy_word b, int kind) {
    if (a == b && kind != FK_FLOAT) return true;
    if (kind == FK_INT || kind == FK_BOOL) return false;
    if (kind == FK_OBJ && a && b) {
        FpyObj *x = fpy_pw(a), *y = fpy_pw(b);
        if (x->ty == &fpy_type_str && y->ty == &fpy_type_str) {
            FpyStr *p = (FpyStr *)x, *q = (FpyStr *)y;
            return p->len == q->len && memcmp(p->data, q->data, (size_t)p->len) == 0;
        }
    }
    return fpy_eq_word(a, b, kind);
}

static void dict_alloc_tables(FpyDict *d, int64_t icap, int64_t ecap) {
    d->icap = icap;
    d->ecap = ecap;
    d->index = (int64_t *)fpy_alloc_words(icap)->items;
    for (int64_t i = 0; i < icap; i++) d->index[i] = -1;
    d->entries = (FpyDictEntry *)fpy_alloc_words(ecap * 3)->items;
}

static FpyDict *dict_make(const FpyType *ty, int kk, int vk) {
    FpyDict *d = fpy_alloc(ty, sizeof(FpyDict));
    d->kkind = kk;
    d->vkind = vk;
    d->is_set = ty == &fpy_type_set;
    dict_alloc_tables(d, 8, 5);
    return d;
}

FpyDict *fpy_dict_new(int kkind, int vkind) { return dict_make(&fpy_type_dict, kkind, vkind); }

static void dict_resize(FpyDict *d) {
    int64_t need = d->len * 3 / 2 + 8;
    int64_t icap = 8;
    while (icap * 2 < need * 3) icap *= 2;
    FpyDictEntry *old = d->entries;
    int64_t oldused = d->used;
    dict_alloc_tables(d, icap, icap * 2 / 3 + 1);
    int64_t j = 0;
    int64_t m = icap - 1;
    for (int64_t i = 0; i < oldused; i++) {
        if (old[i].hash == FPY_DELETED_HASH) continue;
        d->entries[j] = old[i];
        uint64_t p = (uint64_t)old[i].hash & (uint64_t)m;
        while (d->index[p] != -1) p = (p + 1) & (uint64_t)m;
        d->index[p] = j;
        j++;
    }
    d->used = j;
    d->len = j;
}

/* Returns index slot for key; *found set if present. */
static int64_t dict_probe(FpyDict *d, fpy_word key, int64_t h, bool *found) {
    uint64_t m = (uint64_t)d->icap - 1, p = (uint64_t)h & m;
    int64_t first_dummy = -1;
    for (;;) {
        int64_t ix = d->index[p];
        if (ix == -1) {
            *found = false;
            return first_dummy >= 0 ? first_dummy : (int64_t)p;
        }
        if (ix == -2) {
            if (first_dummy < 0) first_dummy = (int64_t)p;
        } else {
            FpyDictEntry *e = &d->entries[ix];
            if (e->hash == h && key_eq(e->key, key, d->kkind)) {
                *found = true;
                return (int64_t)p;
            }
        }
        p = (p + 1) & m;
    }
}

bool fpy_dict_lookup(FpyDict *d, fpy_word key, fpy_word *out) {
    if (d->len == 0) return false;
    bool found;
    int64_t slot = dict_probe(d, key, hash_key(key, d->kkind), &found);
    if (found && out) *out = d->entries[d->index[slot]].value;
    return found;
}

_Noreturn static void key_error(fpy_word key, int kind) {
    /* the key itself is the argument: str(KeyError) is its repr */
    fpy_raise(fpy_make_builtin_exc(FPY_EXC_KEY, (FpyStr *)fpy_box_word(key, kind)));
}

fpy_word fpy_dict_get(FpyDict *d, fpy_word key) {
    fpy_word v;
    if (!fpy_dict_lookup(d, key, &v)) key_error(key, d->kkind);
    return v;
}

void fpy_dict_set(FpyDict *d, fpy_word key, fpy_word value) {
    int64_t h = hash_key(key, d->kkind);
    bool found;
    int64_t slot = dict_probe(d, key, h, &found);
    if (found) {
        d->entries[d->index[slot]].value = value;
        return;
    }
    if (d->used >= d->ecap || (d->used + 1) * 3 > d->icap * 2) {
        dict_resize(d);
        slot = dict_probe(d, key, h, &found);
    }
    FpyDictEntry *e = &d->entries[d->used];
    e->hash = h;
    e->key = key;
    e->value = value;
    d->index[slot] = d->used;
    d->used++;
    d->len++;
}

bool fpy_dict_contains(FpyDict *d, fpy_word key) { return fpy_dict_lookup(d, key, NULL); }

/* Pointer to the value slot for `key`, or NULL if absent (valid until the dict is modified). */
fpy_word *fpy_dict_slot(FpyDict *d, fpy_word key) {
    if (d->len == 0) return NULL;
    bool found;
    int64_t slot = dict_probe(d, key, hash_key(key, d->kkind), &found);
    return found ? &d->entries[d->index[slot]].value : NULL;
}

bool fpy_dict_del(FpyDict *d, fpy_word key, bool raise) {
    bool found = false;
    int64_t slot = d->len ? dict_probe(d, key, hash_key(key, d->kkind), &found) : 0;
    if (!found) {
        if (raise) key_error(key, d->kkind);
        return false;
    }
    FpyDictEntry *e = &d->entries[d->index[slot]];
    e->hash = FPY_DELETED_HASH;
    e->key = 0;
    e->value = 0;
    d->index[slot] = -2;
    d->len--;
    return true;
}

fpy_word fpy_dict_pop(FpyDict *d, fpy_word key, bool has_default, fpy_word dflt) {
    fpy_word v;
    if (!fpy_dict_lookup(d, key, &v)) {
        if (has_default) return dflt;
        key_error(key, d->kkind);
    }
    fpy_dict_del(d, key, false);
    return v;
}

fpy_word fpy_dict_setdefault(FpyDict *d, fpy_word key, fpy_word dflt) {
    fpy_word v;
    if (fpy_dict_lookup(d, key, &v)) return v;
    fpy_dict_set(d, key, dflt);
    return dflt;
}

void fpy_dict_update(FpyDict *d, FpyDict *o) {
    FpyDictEntry *e;
    for (int64_t i = 0; fpy_dict_next(o, &i, &e);) fpy_dict_set(d, e->key, e->value);
}

void fpy_dict_clear(FpyDict *d) {
    dict_alloc_tables(d, 8, 5);
    d->len = d->used = 0;
}

FpyDict *fpy_dict_copy(FpyDict *d) {
    FpyDict *r = dict_make(d->ty, d->kkind, d->vkind);
    fpy_dict_update(r, d);
    return r;
}

FpyList *fpy_dict_keys(FpyDict *d) {
    FpyList *l = fpy_list_new(d->kkind, d->len);
    FpyDictEntry *e;
    for (int64_t i = 0; fpy_dict_next(d, &i, &e);) l->items[l->len++] = e->key;
    return l;
}

FpyList *fpy_dict_values(FpyDict *d) {
    FpyList *l = fpy_list_new(d->vkind, d->len);
    FpyDictEntry *e;
    for (int64_t i = 0; fpy_dict_next(d, &i, &e);) l->items[l->len++] = e->value;
    return l;
}

static FpyTuple *pair(fpy_word a, int ka, fpy_word b, int kb) {
    uint8_t kinds[2] = {(uint8_t)ka, (uint8_t)kb};
    FpyTuple *t = fpy_tuple_new(2, kinds);
    t->items[0] = a;
    t->items[1] = b;
    return t;
}

FpyList *fpy_dict_items(FpyDict *d) {
    FpyList *l = fpy_list_new(FK_OBJ, d->len);
    FpyDictEntry *e;
    for (int64_t i = 0; fpy_dict_next(d, &i, &e);)
        l->items[l->len++] = fpy_wp(pair(e->key, d->kkind, e->value, d->vkind));
    return l;
}

FpyTuple *fpy_dict_popitem(FpyDict *d) {
    if (d->len == 0) fpy_raise_code(FPY_EXC_KEY, "popitem(): dictionary is empty");
    for (int64_t i = d->used - 1; i >= 0; i--) {
        FpyDictEntry *e = &d->entries[i];
        if (e->hash == FPY_DELETED_HASH) continue;
        FpyTuple *t = pair(e->key, d->kkind, e->value, d->vkind);
        fpy_dict_del(d, e->key, false);
        return t;
    }
    fpy_raise_code(FPY_EXC_KEY, "popitem(): dictionary is empty");
}

static bool dict_iter_next(FpyIter *it, fpy_word *out) {
    FpyDict *d = (FpyDict *)it->o1;
    if ((int64_t)it->c != d->len) fpy_raise_code(FPY_EXC_RUNTIME, "dictionary changed size during iteration");
    FpyDictEntry *e;
    int64_t pos = it->i;
    if (!fpy_dict_next(d, &pos, &e)) return false;
    it->i = pos;
    switch (it->a) {
    case 0: *out = e->key; break;
    case 1: *out = e->value; break;
    default: *out = fpy_wp(pair(e->key, d->kkind, e->value, d->vkind));
    }
    return true;
}

static FpyIter *dict_iter(FpyDict *d, int mode, int kind) {
    FpyIter *it = fpy_iter_new(dict_iter_next, kind);
    it->o1 = (FpyObj *)d;
    it->a = (fpy_word)mode;
    it->n = d->used;
    it->c = (fpy_word)d->len;
    return it;
}

FpyIter *fpy_dict_iter_keys(FpyDict *d) { return dict_iter(d, 0, d->kkind); }
FpyIter *fpy_dict_iter_values(FpyDict *d) { return dict_iter(d, 1, d->vkind); }
FpyIter *fpy_dict_iter_items(FpyDict *d) { return dict_iter(d, 2, FK_OBJ); }

bool fpy_dict_eq(FpyDict *a, FpyDict *b) {
    if (a == b) return true;
    if (a->len != b->len) return false;
    FpyDictEntry *e;
    for (int64_t i = 0; fpy_dict_next(a, &i, &e);) {
        fpy_word v;
        if (!fpy_dict_lookup(b, e->key, &v)) return false;
        if (!a->is_set && !fpy_eq_word(e->value, v, a->vkind)) return false;
    }
    return true;
}

/* ================================================================== set */
FpySet *fpy_set_new(int kind) { return dict_make(&fpy_type_set, kind, FK_INT); }
void fpy_set_add(FpySet *s, fpy_word v) { fpy_dict_set(s, v, 0); }
bool fpy_set_contains(FpySet *s, fpy_word v) { return fpy_dict_lookup(s, v, NULL); }
void fpy_set_discard(FpySet *s, fpy_word v) { fpy_dict_del(s, v, false); }
void fpy_set_remove(FpySet *s, fpy_word v) { fpy_dict_del(s, v, true); }
fpy_word fpy_set_pop(FpySet *s) {
    if (s->len == 0) fpy_raise_code(FPY_EXC_KEY, "pop from an empty set");
    FpyDictEntry *e;
    int64_t i = 0;
    fpy_dict_next(s, &i, &e);
    fpy_word k = e->key;
    fpy_dict_del(s, k, false);
    return k;
}
FpySet *fpy_set_union(FpySet *a, FpySet *b) {
    FpySet *r = fpy_dict_copy(a);
    fpy_dict_update(r, b);
    return r;
}
FpySet *fpy_set_intersection(FpySet *a, FpySet *b) {
    FpySet *r = fpy_set_new(a->kkind);
    FpyDictEntry *e;
    for (int64_t i = 0; fpy_dict_next(a, &i, &e);)
        if (fpy_set_contains(b, e->key)) fpy_set_add(r, e->key);
    return r;
}
FpySet *fpy_set_difference(FpySet *a, FpySet *b) {
    FpySet *r = fpy_set_new(a->kkind);
    FpyDictEntry *e;
    for (int64_t i = 0; fpy_dict_next(a, &i, &e);)
        if (!fpy_set_contains(b, e->key)) fpy_set_add(r, e->key);
    return r;
}
FpySet *fpy_set_symdiff(FpySet *a, FpySet *b) {
    FpySet *r = fpy_set_difference(a, b);
    FpyDictEntry *e;
    for (int64_t i = 0; fpy_dict_next(b, &i, &e);)
        if (!fpy_set_contains(a, e->key)) fpy_set_add(r, e->key);
    return r;
}
bool fpy_set_issubset(FpySet *a, FpySet *b) {
    FpyDictEntry *e;
    for (int64_t i = 0; fpy_dict_next(a, &i, &e);)
        if (!fpy_set_contains(b, e->key)) return false;
    return true;
}
FpyIter *fpy_set_iter(FpySet *s) { return dict_iter(s, 0, s->kkind); }
FpySet *fpy_set_from_iter(int kind, FpyIter *it) {
    FpySet *s = fpy_set_new(kind);
    fpy_word w;
    while (it->next(it, &w)) fpy_set_add(s, w);
    return s;
}

/* ================================================================== tuple */
static void tuple_trace(FpyObj *o) {
    FpyTuple *t = (FpyTuple *)o;
    if (t->kinds) fpy_gc_mark(t->kinds);
    for (int64_t i = 0; i < t->len; i++)
        if (t->kinds[i] == FK_OBJ && t->items[i]) fpy_gc_mark(fpy_pw(t->items[i]));
}

const FpyType fpy_type_tuple = {.name = "tuple", .tag = FT_TUPLE, .trace = tuple_trace};

FpyTuple *fpy_tuple_new(int64_t n, const uint8_t *kinds) {
    FpyTuple *t = fpy_alloc(&fpy_type_tuple, sizeof(FpyTuple) + (size_t)n * sizeof(fpy_word));
    t->len = n;
    uint8_t *k = fpy_alloc_raw((size_t)n + 1);
    if (kinds) memcpy(k, kinds, (size_t)n);
    else memset(k, FK_OBJ, (size_t)n);
    t->kinds = k;
    return t;
}

/* tuple[T, ...] support: all items share one kind */
FpyTuple *fpy_tuple_uniform(int64_t n, int kind) {
    FpyTuple *t = fpy_tuple_new(n, NULL);
    memset(t->kinds, kind, (size_t)n);
    return t;
}

FpyTuple *fpy_tuple_from_list(FpyList *l) {
    FpyTuple *t = fpy_tuple_uniform(l->len, l->kind);
    memcpy(t->items, l->items, (size_t)l->len * sizeof(fpy_word));
    return t;
}

FpyTuple *fpy_tuple_from_iter(int kind, FpyIter *it) { return fpy_tuple_from_list(fpy_list_from_iter(kind, it)); }

/* A tuple whose items all have `kind` (converting boxed/unboxed items; checked). */
FpyTuple *fpy_tuple_retype(FpyTuple *t, int kind) {
    bool same = true;
    for (int64_t i = 0; i < t->len && same; i++) same = t->kinds[i] == kind;
    if (same) return t;
    FpyTuple *r = fpy_tuple_uniform(t->len, kind);
    static const char *names[] = {"int", "float", "bool", "object"};
    for (int64_t i = 0; i < t->len; i++) {
        fpy_word w = t->items[i];
        if (kind == FK_OBJ) r->items[i] = fpy_wp(fpy_box_word(w, t->kinds[i]));
        else if (t->kinds[i] == FK_OBJ) r->items[i] = fpy_unbox_word(fpy_pw(w), kind, names[kind]);
        else if (kind == FK_FLOAT && t->kinds[i] == FK_INT) r->items[i] = fpy_wf((double)(fpy_int)w);
        else if (kind == FK_INT && t->kinds[i] == FK_BOOL) r->items[i] = w;
        else fpy_raise_fmt(FPY_EXC_TYPE, "tuple item %lld is not %s", (long long)i, names[kind]);
    }
    return r;
}

fpy_word fpy_tuple_get(FpyTuple *t, fpy_int i) { return t->items[fpy_norm_index(i, t->len, "tuple")]; }

FpyTuple *fpy_tuple_slice(FpyTuple *t, bool has_lo, fpy_int lo, bool has_hi, fpy_int hi, fpy_int step) {
    int64_t start, stop;
    fpy_slice_indices(t->len, has_lo, lo, has_hi, hi, step, &start, &stop);
    int64_t n = fpy_slice_len(start, stop, step);
    FpyTuple *r = fpy_tuple_new(n, NULL);
    for (int64_t k = 0, i = start; k < n; k++, i += step) {
        r->items[k] = t->items[i];
        r->kinds[k] = t->kinds[i];
    }
    return r;
}

FpyTuple *fpy_tuple_concat(FpyTuple *a, FpyTuple *b) {
    FpyTuple *r = fpy_tuple_new(a->len + b->len, NULL);
    memcpy(r->items, a->items, (size_t)a->len * sizeof(fpy_word));
    memcpy(r->items + a->len, b->items, (size_t)b->len * sizeof(fpy_word));
    memcpy(r->kinds, a->kinds, (size_t)a->len);
    memcpy(r->kinds + a->len, b->kinds, (size_t)b->len);
    return r;
}

bool fpy_tuple_contains(FpyTuple *t, fpy_word v, int kind) {
    for (int64_t i = 0; i < t->len; i++) {
        bool eq = t->kinds[i] == kind ? fpy_eq_word(t->items[i], v, kind) : fpy_obj_eq(fpy_box_word(t->items[i], t->kinds[i]), fpy_box_word(v, kind));
        if (eq) return true;
    }
    return false;
}

bool fpy_tuple_eq(FpyTuple *a, FpyTuple *b) {
    if (a->len != b->len) return false;
    for (int64_t i = 0; i < a->len; i++) {
        if (a->kinds[i] != b->kinds[i]) {
            FpyObj *x = fpy_box_word(a->items[i], a->kinds[i]), *y = fpy_box_word(b->items[i], b->kinds[i]);
            if (!fpy_obj_eq(x, y)) return false;
        } else if (!fpy_eq_word(a->items[i], b->items[i], a->kinds[i])) return false;
    }
    return true;
}

int fpy_tuple_cmp(FpyTuple *a, FpyTuple *b) {
    int64_t n = a->len < b->len ? a->len : b->len;
    for (int64_t i = 0; i < n; i++) {
        int c = fpy_cmp_word(a->items[i], b->items[i], a->kinds[i]);
        if (c) return c;
    }
    return a->len < b->len ? -1 : (a->len > b->len);
}

static bool tuple_iter_next(FpyIter *it, fpy_word *out) {
    FpyTuple *t = (FpyTuple *)it->o1;
    if (it->i >= t->len) return false;
    *out = t->items[it->i++];
    return true;
}

FpyIter *fpy_tuple_iter(FpyTuple *t) {
    FpyIter *it = fpy_iter_new(tuple_iter_next, t->len ? t->kinds[0] : FK_OBJ);
    it->o1 = (FpyObj *)t;
    return it;
}

/* Convert containers crossing an Any boundary into the element representation typed code expects. */
static fpy_word rekind(fpy_word w, int from, int to) {
    if (from == to) return w;
    FpyObj *b = fpy_box_word(w, from);
    return fpy_unbox_word(b, to, to == FK_INT ? "int" : to == FK_FLOAT ? "float" : to == FK_BOOL ? "bool" : "object");
}

FpyList *fpy_retype_list(FpyList *l, int kind) {
    if (!l || l->kind == kind) return l;
    FpyList *r = fpy_list_new(kind, l->len);
    for (int64_t i = 0; i < l->len; i++) fpy_list_append(r, rekind(l->items[i], l->kind, kind));
    return r;
}

FpyDict *fpy_retype_dict(FpyDict *d, int kk, int vk) {
    if (!d || (d->kkind == kk && d->vkind == vk)) return d;
    FpyDict *r = d->is_set ? fpy_set_new(kk) : fpy_dict_new(kk, vk);
    FpyDictEntry *e;
    for (int64_t i = 0; fpy_dict_next(d, &i, &e);)
        fpy_dict_set(r, rekind(e->key, d->kkind, kk), d->is_set ? 0 : rekind(e->value, d->vkind, vk));
    return r;
}
