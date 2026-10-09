/* fpython runtime — generators as stackful coroutines.
 * A tiny assembly context switch (x86-64 SysV and AArch64) keeps this portable to musl,
 * which has no ucontext. Suspended generator stacks are traced conservatively by the GC
 * when the generator object itself is reachable.
 */
#include "fpy.h"
#include <sys/mman.h>

#if defined(__APPLE__)
#define SYM(x) "_" #x
#else
#define SYM(x) #x
#endif

void fpy_ctx_switch(void **save_sp, void *new_sp);

#if defined(__x86_64__)
__asm__(".text\n"
        ".globl " SYM(fpy_ctx_switch) "\n"
        SYM(fpy_ctx_switch) ":\n"
        "  pushq %rbp\n  pushq %rbx\n  pushq %r12\n  pushq %r13\n  pushq %r14\n  pushq %r15\n"
        "  movq %rsp, (%rdi)\n"
        "  movq %rsi, %rsp\n"
        "  popq %r15\n  popq %r14\n  popq %r13\n  popq %r12\n  popq %rbx\n  popq %rbp\n"
        "  ret\n");
#elif defined(__aarch64__)
__asm__(".text\n"
        ".globl " SYM(fpy_ctx_switch) "\n"
        ".p2align 2\n"
        SYM(fpy_ctx_switch) ":\n"
        "  sub sp, sp, #176\n"
        "  stp x19, x20, [sp, #0]\n  stp x21, x22, [sp, #16]\n  stp x23, x24, [sp, #32]\n"
        "  stp x25, x26, [sp, #48]\n  stp x27, x28, [sp, #64]\n  stp x29, x30, [sp, #80]\n"
        "  stp d8, d9, [sp, #96]\n  stp d10, d11, [sp, #112]\n  stp d12, d13, [sp, #128]\n  stp d14, d15, [sp, #144]\n"
        "  mov x9, sp\n  str x9, [x0]\n  mov sp, x1\n"
        "  ldp x19, x20, [sp, #0]\n  ldp x21, x22, [sp, #16]\n  ldp x23, x24, [sp, #32]\n"
        "  ldp x25, x26, [sp, #48]\n  ldp x27, x28, [sp, #64]\n  ldp x29, x30, [sp, #80]\n"
        "  ldp d8, d9, [sp, #96]\n  ldp d10, d11, [sp, #112]\n  ldp d12, d13, [sp, #128]\n  ldp d14, d15, [sp, #144]\n"
        "  add sp, sp, #176\n"
        "  ret\n");
#else
#error "fpython generators: unsupported architecture"
#endif

#define GEN_STACK (256 * 1024)

struct FpyGen {
    FpyIter it; /* must be first: generators are iterators */
    char *stack;
    char *stack_top;
    void *sp;        /* saved generator sp while suspended */
    void *caller_sp; /* saved resumer sp while running */
    FpyGenBody body;
    void *frame;
    fpy_word value;
    int32_t vkind;
    bool started, done, running;
    FpyExcFrame *exc_top;    /* generator's exception chain while suspended */
    FpyExcFrame *caller_exc; /* resumer's chain while running */
    FpyGen *prev;            /* resumer generator (NULL = main stack) */
    FpyObj *pending_exc;
    /* coroutine support */
    fpy_word sent;    /* value passed by send() (object) */
    fpy_word result;  /* return value of a coroutine (object) */
    FpyObj *thrown;   /* exception to raise at the suspension point */
};

static _Thread_local FpyGen *current_gen;
extern void fpy_gc_scan_range_pub(void *lo, void *hi);

static void gen_trace(FpyObj *o) {
    FpyGen *g = (FpyGen *)o;
    if (g->frame) fpy_gc_mark(g->frame);
    if (g->vkind == FK_OBJ && g->value) fpy_gc_mark(fpy_pw(g->value));
    if (g->pending_exc) fpy_gc_mark(g->pending_exc);
    if (g->sent) fpy_gc_mark(fpy_pw(g->sent));
    if (g->result) fpy_gc_mark(fpy_pw(g->result));
    if (g->thrown) fpy_gc_mark(g->thrown);
    if (g->stack && g->started && !g->done && !g->running) fpy_gc_scan_range_pub(g->sp, g->stack_top);
}

/* Pool of released coroutine stacks (avoids an mmap/munmap pair per short-lived generator). */
static char *stack_pool[64];
static int nstack_pool;

static void release_stack(char *st) {
    if (nstack_pool < 64) stack_pool[nstack_pool++] = st;
    else munmap(st, GEN_STACK);
}

static char *acquire_stack(void) {
    if (nstack_pool) return stack_pool[--nstack_pool];
    char *st = mmap(NULL, GEN_STACK, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
    if (st == MAP_FAILED) return NULL;
    mprotect(st, 4096, PROT_NONE); /* guard page */
    return st;
}

static void gen_finalize(FpyObj *o) {
    FpyGen *g = (FpyGen *)o;
    if (g->stack) {
        release_stack(g->stack);
        g->stack = NULL;
    }
}

const FpyType fpy_type_gen = {.name = "generator", .tag = FT_ITER, .trace = gen_trace, .finalize = gen_finalize};

/* Called by the GC: scan the stacks of the chain of running generators and their resumers. */
void fpy_gen_scan_running(void *cur_sp, void *main_bottom) {
    if (!current_gen) {
        fpy_gc_scan_range_pub(cur_sp, main_bottom);
        return;
    }
    fpy_gc_scan_range_pub(cur_sp, current_gen->stack_top);
    for (FpyGen *g = current_gen; g; g = g->prev) {
        fpy_gc_mark(g);
        fpy_gc_scan_range_pub(g->caller_sp, g->prev ? g->prev->stack_top : main_bottom);
    }
}

void fpy_gen_mark_roots(void) {
    for (FpyGen *g = current_gen; g; g = g->prev) fpy_gc_mark(g);
}

static void gen_trampoline(void) {
    FpyGen *g = current_gen;
    FpyExcFrame f;
    if (FPY_TRY(f)) {
        g->body(g);
        FPY_POP(f);
    } else {
        FPY_POP(f);
        g->pending_exc = fpy_cur_exc;
    }
    g->done = true;
    g->value = 0;
    void *dummy;
    current_gen = g->prev;
    g->running = false;
    fpy_exc_top = g->caller_exc;
    fpy_ctx_switch(&dummy, g->caller_sp);
    __builtin_unreachable();
}

static void gen_resume(FpyGen *g) {
    if (g->running) fpy_raise_code(FPY_EXC_VALUE, "generator already executing");
    if (!g->started) {
        g->started = true;
        g->stack = acquire_stack();
        if (!g->stack) fpy_raise_code(FPY_EXC_MEMORY, "cannot allocate generator stack");
        g->stack_top = g->stack + GEN_STACK;
        uintptr_t top = (uintptr_t)g->stack_top & ~(uintptr_t)15;
#if defined(__x86_64__)
        void **sp = (void **)top;
        *--sp = NULL;                    /* alignment / fake return address */
        *--sp = (void *)gen_trampoline;  /* popped by ret */
        for (int i = 0; i < 6; i++) *--sp = NULL;
        g->sp = sp;
#else
        char *sp = (char *)top - 176;
        memset(sp, 0, 176);
        ((void **)sp)[11] = (void *)gen_trampoline; /* x30 slot at offset 88 */
        g->sp = sp;
#endif
        g->exc_top = NULL;
    }
    g->prev = current_gen;
    g->caller_exc = fpy_exc_top;
    fpy_exc_top = g->exc_top;
    current_gen = g;
    g->running = true;
    fpy_ctx_switch(&g->caller_sp, g->sp);
    /* back in the resumer */
    if (g->pending_exc) {
        FpyObj *e = g->pending_exc;
        g->pending_exc = NULL;
        gen_finalize((FpyObj *)g);
        fpy_raise(e);
    }
    if (g->done) gen_finalize((FpyObj *)g);
}

void fpy_gen_yield(FpyGen *g, fpy_word v) {
    g->value = v;
    g->exc_top = fpy_exc_top;
    fpy_exc_top = g->caller_exc;
    current_gen = g->prev;
    g->running = false;
    fpy_ctx_switch(&g->sp, g->caller_sp);
    /* resumed */
}

static bool gen_next(FpyIter *it, fpy_word *out) {
    FpyGen *g = (FpyGen *)it;
    if (g->done) return false;
    gen_resume(g);
    if (g->done) return false;
    *out = g->value;
    return true;
}

FpyIter *fpy_gen_new(FpyGenBody body, void *frame, int kind) {
    FpyGen *g = fpy_alloc(&fpy_type_gen, sizeof(FpyGen));
    g->it.next = gen_next;
    g->it.kind = kind;
    g->body = body;
    g->frame = frame;
    g->vkind = kind;
    return (FpyIter *)g;
}

void *fpy_gen_frame(FpyGen *g) { return g->frame; }

/* ------------------------------------------------------------------ coroutine API */
/* Resume with a sent value: returns 1 and sets *out if it yielded, 0 if it finished. */
int fpy_gen_step(FpyIter *it, fpy_word sent, FpyObj *thrown, fpy_word *out) {
    FpyGen *g = (FpyGen *)it;
    if (g->done) return 0;
    g->sent = sent;
    g->thrown = thrown;
    gen_resume(g);
    if (g->done) return 0;
    *out = g->value;
    return 1;
}
fpy_word fpy_gen_take_sent(FpyGen *g) { fpy_word v = g->sent; g->sent = 0; return v; }

/* ------------------------------------------------------------------ generator methods */
/* `x = yield v` in native code: suspend, then raise a throw()n exception or return the sent
 * value (a boxed object, NULL for next()). */
fpy_word fpy_gen_yield_recv(FpyGen *g, fpy_word v) {
    fpy_gen_yield(g, v);
    FpyObj *t = fpy_gen_take_thrown(g);
    if (t) fpy_raise(t);
    return fpy_gen_take_sent(g);
}

static FpyGen *as_gen(FpyIter *it, const char *what) {
    if (!it || it->ty != &fpy_type_gen) fpy_raise_fmt(FPY_EXC_TYPE, "%s() needs a generator", what);
    return (FpyGen *)it;
}

fpy_word fpy_gen_send(FpyIter *it, FpyObj *v) {
    FpyGen *g = as_gen(it, "send");
    if (!g->started && v) fpy_raise_code(FPY_EXC_TYPE, "can't send non-None value to a just-started generator");
    fpy_word out;
    if (!fpy_gen_step(it, fpy_wp(v), NULL, &out)) fpy_raise_stop_iteration();
    return out;
}

fpy_word fpy_gen_throw(FpyIter *it, FpyObj *exc) {
    FpyGen *g = as_gen(it, "throw");
    if (!g->started || g->done) {
        g->done = true;
        fpy_raise(exc);
    }
    fpy_word out;
    if (!fpy_gen_step(it, 0, exc, &out)) fpy_raise_stop_iteration();
    return out;
}

void fpy_gen_close(FpyIter *it) {
    FpyGen *g = as_gen(it, "close");
    if (g->done) return;
    if (!g->started) {
        g->done = true;
        return;
    }
    FpyObj *gexit = fpy_make_builtin_exc(FPY_EXC_GENEXIT, fpy_str_from_cstr(""));
    FpyExcFrame f;
    if (FPY_TRY(f)) {
        fpy_word out;
        int yielded = fpy_gen_step(it, 0, gexit, &out);
        FPY_POP(f);
        if (yielded) fpy_raise_code(FPY_EXC_RUNTIME, "generator ignored GeneratorExit");
    } else {
        FPY_POP(f);
        FpyObj *e = fpy_cur_exc;
        if (e != gexit && !fpy_exc_match(e, fpy_builtin_exc_type(FPY_EXC_STOPITER))) fpy_reraise();
    }
}
FpyObj *fpy_gen_take_thrown(FpyGen *g) { FpyObj *t = g->thrown; g->thrown = NULL; return t; }
void fpy_gen_set_result(FpyGen *g, fpy_word r) { g->result = r; }
fpy_word fpy_gen_result(FpyIter *it) { return ((FpyGen *)it)->result; }
bool fpy_gen_done(FpyIter *it) { return ((FpyGen *)it)->done; }
