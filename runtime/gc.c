/* fpython runtime — mark & sweep garbage collector.
 * Heap objects are traced precisely through their FpyType; native stacks (main stack and
 * generator stacks) and registers are scanned conservatively.
 */
#include "fpy.h"
#include <stdint.h>

#define FPY_BLOCK_SHIFT 16
#define FPY_BLOCK_SIZE (1u << FPY_BLOCK_SHIFT)
#define FPY_LARGE_LIMIT 2048

static const uint32_t size_classes[] = {16, 32, 48, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
                                        384, 448, 512, 640, 768, 896, 1024, 1280, 1536, 2048};
#define NCLASSES (sizeof(size_classes) / sizeof(size_classes[0]))
static uint8_t class_for_16[FPY_LARGE_LIMIT / 16 + 1];

typedef struct Block {
    char *base;
    uint32_t objsize, nobjs, nfree, cls;
    uint8_t *alloc;
    uint8_t *mark;
    void *freelist;
    struct Block *next; /* blocks of the same class */
} Block;

typedef struct Large {
    char *base; /* object start */
    size_t size;
    bool mark;
} Large;

typedef struct StackRange { void *lo, *hi; } StackRange;

static Block **btab;  /* hash: block base -> Block */
static size_t btab_cap, btab_len;
static Block *class_blocks[NCLASSES];
static Block *class_cur[NCLASSES];
static Large *larges;
static size_t nlarge, cap_large;
static uintptr_t heap_lo = UINTPTR_MAX, heap_hi = 0;
static void **mstack;
static size_t mstack_len, mstack_cap;
static StackRange roots[256];
static int nroots;
static StackRange stacks[4096];
static int nstacks;
static void *stack_bottom;
static size_t bytes_since_gc, live_bytes, threshold = 8u << 20;
static bool gc_disabled;

static inline size_t bhash(uintptr_t base) { return (size_t)((base >> FPY_BLOCK_SHIFT) * 0x9E3779B97F4A7C15ull); }

static void btab_insert(Block *b);
static void btab_grow(void) {
    Block **old = btab;
    size_t oldcap = btab_cap;
    btab_cap = btab_cap ? btab_cap * 2 : 1024;
    btab = calloc(btab_cap, sizeof(Block *));
    btab_len = 0;
    for (size_t i = 0; i < oldcap; i++)
        if (old[i]) btab_insert(old[i]);
    free(old);
}
static void btab_insert(Block *b) {
    if ((btab_len + 1) * 2 > btab_cap) btab_grow();
    size_t m = btab_cap - 1, i = bhash((uintptr_t)b->base) & m;
    while (btab[i]) i = (i + 1) & m;
    btab[i] = b;
    btab_len++;
}
static inline Block *btab_find(uintptr_t base) {
    if (!btab_cap) return NULL;
    size_t m = btab_cap - 1, i = bhash(base) & m;
    while (btab[i]) {
        if ((uintptr_t)btab[i]->base == base) return btab[i];
        i = (i + 1) & m;
    }
    return NULL;
}

void fpy_gc_init(void *bottom) {
    stack_bottom = bottom;
    for (size_t i = 0, c = 0; i <= FPY_LARGE_LIMIT / 16; i++) {
        while (size_classes[c] < i * 16) c++;
        class_for_16[i] = (uint8_t)c;
    }
    const char *e = getenv("FPY_GC_THRESHOLD_MB");
    if (e) threshold = (size_t)atol(e) << 20;
    if (getenv("FPY_GC_DISABLE")) gc_disabled = true;
}

void fpy_gc_add_root(void *start, size_t bytes) {
    if (nroots >= 256) { fprintf(stderr, "fpython: too many GC roots\n"); abort(); }
    roots[nroots].lo = start;
    roots[nroots].hi = (char *)start + bytes;
    nroots++;
}

void fpy_gc_register_stack(void *lo, void *hi) {
    for (int i = 0; i < nstacks; i++)
        if (stacks[i].lo == NULL || stacks[i].hi == hi) { stacks[i].lo = lo; stacks[i].hi = hi; return; }
    if (nstacks >= 4096) { fprintf(stderr, "fpython: too many live generators\n"); abort(); }
    stacks[nstacks].lo = lo;
    stacks[nstacks].hi = hi;
    nstacks++;
}

void fpy_gc_unregister_stack(void *hi) {
    for (int i = 0; i < nstacks; i++)
        if (stacks[i].hi == hi) { stacks[i].lo = NULL; stacks[i].hi = NULL; }
}

static Block *new_block(uint32_t cls) {
    void *mem = NULL;
    if (posix_memalign(&mem, FPY_BLOCK_SIZE, FPY_BLOCK_SIZE) != 0 || !mem) {
        fprintf(stderr, "fpython: out of memory\n");
        abort();
    }
    Block *b = calloc(1, sizeof(Block));
    b->base = mem;
    b->cls = cls;
    b->objsize = size_classes[cls];
    b->nobjs = FPY_BLOCK_SIZE / b->objsize;
    b->alloc = calloc(b->nobjs, 1);
    b->mark = calloc(b->nobjs, 1);
    /* build free list in address order */
    void *fl = NULL;
    for (int64_t i = (int64_t)b->nobjs - 1; i >= 0; i--) {
        void **o = (void **)(b->base + (size_t)i * b->objsize);
        *o = fl;
        fl = o;
    }
    b->freelist = fl;
    b->nfree = b->nobjs;
    b->next = class_blocks[cls];
    class_blocks[cls] = b;
    btab_insert(b);
    if ((uintptr_t)mem < heap_lo) heap_lo = (uintptr_t)mem;
    if ((uintptr_t)mem + FPY_BLOCK_SIZE > heap_hi) heap_hi = (uintptr_t)mem + FPY_BLOCK_SIZE;
    return b;
}

static void maybe_collect(size_t size) {
    bytes_since_gc += size;
    if (!gc_disabled && bytes_since_gc > threshold && bytes_since_gc > live_bytes) fpy_gc_collect();
}

static void *alloc_small(size_t size) {
    uint32_t cls = class_for_16[(size + 15) / 16];
    Block *b = class_cur[cls];
    if (!b || !b->freelist) {
        maybe_collect(FPY_BLOCK_SIZE / 4);
        b = NULL;
        for (Block *x = class_blocks[cls]; x; x = x->next)
            if (x->freelist) { b = x; break; }
        if (!b) b = new_block(cls);
        class_cur[cls] = b;
    } else {
        bytes_since_gc += b->objsize;
    }
    void **o = b->freelist;
    b->freelist = *o;
    b->nfree--;
    b->alloc[((char *)o - b->base) / b->objsize] = 1;
    memset(o, 0, b->objsize);
    return o;
}

static void *alloc_large(size_t size) {
    maybe_collect(size);
    char *mem = calloc(1, size);
    if (!mem) { fprintf(stderr, "fpython: out of memory\n"); abort(); }
    if (nlarge == cap_large) {
        cap_large = cap_large ? cap_large * 2 : 64;
        larges = realloc(larges, cap_large * sizeof(Large));
    }
    /* keep sorted by address */
    size_t lo = 0, hi = nlarge;
    while (lo < hi) {
        size_t mid = (lo + hi) / 2;
        if (larges[mid].base < mem) lo = mid + 1; else hi = mid;
    }
    memmove(&larges[lo + 1], &larges[lo], (nlarge - lo) * sizeof(Large));
    larges[lo].base = mem;
    larges[lo].size = size;
    larges[lo].mark = false;
    nlarge++;
    if ((uintptr_t)mem < heap_lo) heap_lo = (uintptr_t)mem;
    if ((uintptr_t)mem + size > heap_hi) heap_hi = (uintptr_t)mem + size;
    return mem;
}

void *fpy_alloc(const FpyType *ty, size_t size) {
    if (size < sizeof(FpyObj)) size = sizeof(FpyObj);
    FpyObj *o = size <= FPY_LARGE_LIMIT ? alloc_small(size) : alloc_large(size);
    o->ty = ty;
    return o;
}

const FpyType fpy_type_raw = {.name = "raw", .tag = FT_RAW};

void *fpy_alloc_raw(size_t bytes) {
    FpyObj *o = fpy_alloc(&fpy_type_raw, sizeof(FpyObj) + bytes);
    return o + 1;
}

FpyRaw *fpy_alloc_words(int64_t n) {
    FpyRaw *r = fpy_alloc(&fpy_type_raw, sizeof(FpyRaw) + (size_t)n * sizeof(fpy_word));
    r->cap = n;
    return r;
}

/* Find the start of the object containing p, or NULL. Sets *mark to its mark byte. */
static inline void *find_object(uintptr_t p, bool **lmark, uint8_t **smark) {
    if (p < heap_lo || p >= heap_hi) return NULL;
    Block *b = btab_find(p & ~(uintptr_t)(FPY_BLOCK_SIZE - 1));
    if (b) {
        size_t idx = (p - (uintptr_t)b->base) / b->objsize;
        if (idx >= b->nobjs || !b->alloc[idx]) return NULL;
        *smark = &b->mark[idx];
        return b->base + idx * b->objsize;
    }
    if (!nlarge) return NULL;
    size_t lo = 0, hi = nlarge;
    while (lo < hi) {
        size_t mid = (lo + hi) / 2;
        if ((uintptr_t)larges[mid].base <= p) lo = mid + 1; else hi = mid;
    }
    if (lo == 0) return NULL;
    Large *l = &larges[lo - 1];
    if (p < (uintptr_t)l->base + l->size) {
        *lmark = &l->mark;
        return l->base;
    }
    return NULL;
}

static inline void push_mark(void *o) {
    if (mstack_len == mstack_cap) {
        mstack_cap = mstack_cap ? mstack_cap * 2 : 4096;
        mstack = realloc(mstack, mstack_cap * sizeof(void *));
    }
    mstack[mstack_len++] = o;
}

void fpy_gc_mark(const void *p) {
    bool *lm = NULL;
    uint8_t *sm = NULL;
    void *o = find_object((uintptr_t)p, &lm, &sm);
    if (!o) return;
    if (sm) { if (*sm) return; *sm = 1; }
    else { if (*lm) return; *lm = true; }
    push_mark(o);
}

void fpy_gc_mark_word(fpy_word w, int kind) {
    if (kind == FK_OBJ && w) fpy_gc_mark(fpy_pw(w));
}

static void scan_range(void *lo, void *hi) {
    uintptr_t a = ((uintptr_t)lo + 7) & ~(uintptr_t)7;
    for (; a + sizeof(void *) <= (uintptr_t)hi; a += sizeof(void *)) {
        uintptr_t w = *(uintptr_t *)a;
        if (w >= heap_lo && w < heap_hi) fpy_gc_mark((void *)w);
    }
}

void fpy_gc_scan_range_pub(void *lo, void *hi) { scan_range(lo, hi); }

static void trace(FpyObj *o) {
    const FpyType *t = o->ty;
    if (!t) return;
    if (t->trace) { t->trace(o); return; }
    for (int32_t i = 0; i < t->nptrs; i++) {
        void *p = *(void **)((char *)o + t->ptr_offsets[i]);
        if (p) fpy_gc_mark(p);
    }
}

static void drain(void) {
    while (mstack_len) trace(mstack[--mstack_len]);
}

extern void fpy_rt_mark_roots(void);

__attribute__((noinline)) static void mark_stack(void) {
    jmp_buf regs;
    setjmp(regs); /* spill callee-saved registers onto the stack */
    void *here = &regs;
    extern void fpy_gen_scan_running(void *cur_sp, void *main_bottom);
    void *bottom = stack_bottom;
    if (fpy_py) {
        void *tb = fpy_py->thread_bottom();
        if (tb) bottom = tb;
        fpy_py->scan_threads();
    }
    fpy_gen_scan_running(here, bottom);
    scan_range(&regs, (char *)&regs + sizeof(regs));
}

void fpy_gc_collect(void) {
    for (int i = 0; i < nroots; i++) scan_range(roots[i].lo, roots[i].hi);
    fpy_rt_mark_roots();
    mark_stack();
    for (int i = 0; i < nstacks; i++)
        if (stacks[i].lo) scan_range(stacks[i].lo, stacks[i].hi);
    drain();
    /* sweep */
    size_t live = 0;
    for (uint32_t c = 0; c < NCLASSES; c++) {
        for (Block *b = class_blocks[c]; b; b = b->next) {
            void *fl = NULL;
            uint32_t nfree = 0;
            for (int64_t i = (int64_t)b->nobjs - 1; i >= 0; i--) {
                if (b->alloc[i] && b->mark[i]) {
                    b->mark[i] = 0;
                    live += b->objsize;
                    continue;
                }
                if (b->alloc[i]) {
                    FpyObj *dead = (FpyObj *)(b->base + (size_t)i * b->objsize);
                    if (dead->ty && dead->ty->finalize) dead->ty->finalize(dead);
                }
                b->alloc[i] = 0;
                void **o = (void **)(b->base + (size_t)i * b->objsize);
                *o = fl;
                fl = o;
                nfree++;
            }
            b->freelist = fl;
            b->nfree = nfree;
        }
        class_cur[c] = NULL;
    }
    size_t j = 0;
    for (size_t i = 0; i < nlarge; i++) {
        if (larges[i].mark) {
            larges[i].mark = false;
            live += larges[i].size;
            larges[j++] = larges[i];
        } else {
            FpyObj *dead = (FpyObj *)larges[i].base;
            if (dead->ty && dead->ty->finalize) dead->ty->finalize(dead);
            free(larges[i].base);
        }
    }
    nlarge = j;
    live_bytes = live;
    bytes_since_gc = 0;
    if (threshold < live) threshold = live;
}
