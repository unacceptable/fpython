/* fpython — API used by natively compiled Python modules (compiled as separate objects). */
#ifndef FPY_PYC_API_H
#define FPY_PYC_API_H
#define PY_SSIZE_T_CLEAN
#include <Python.h>

typedef struct FpyGen FpyGen;
typedef PyObject *(*FpcImpl)(PyObject *self, PyObject **slots);

typedef struct FpcSpec {
    const char *name;
    const char *qualname;
    FpcImpl impl;
    int nposonly, npos, nkwonly; /* npos includes posonly */
    int has_varargs, has_varkw;
    const char *const *params; /* npos + nkwonly names, then *args name, then **kw name */
    const char *const *annots; /* annotation source per param (NULL = none), same order */
    const char *ret_annot;     /* return annotation source or NULL */
    int is_coroutine;          /* `async def` */
} FpcSpec;

typedef PyObject *(*FpcGenBody)(PyObject *self, PyObject **slots, FpyGen *g);

PyObject *fpc_make_function(const FpcSpec *spec, PyObject *globals, PyObject *defaults, PyObject *kwdefaults, PyObject *closure, PyObject *doc);
PyObject *fpc_closure_cell(PyObject *fn, int i);
PyObject *fpc_globals(PyObject *fn);
PyObject *fpc_load_global(PyObject *globals, PyObject *name);
PyObject *fpc_load_name(PyObject *ns, PyObject *globals, PyObject *name);
PyObject *fpc_unbound_local(const char *name);
PyObject *fpc_import(PyObject *globals, const char *name, PyObject *fromlist, int level);
PyObject *fpc_import_from(PyObject *module, PyObject *name);
int fpc_import_star(PyObject *module, PyObject *ns);
PyObject *fpc_unpack(PyObject *seq, Py_ssize_t before, int has_star, Py_ssize_t after);
PyObject *fpc_call_star(PyObject *f, PyObject *args_list, PyObject *kwargs);
PyObject *fpc_inplace_pow(PyObject *a, PyObject *b);
int fpc_exc_matches(PyObject *exc, PyObject *cls);
int fpc_raise(PyObject *exc, PyObject *cause);
PyObject *fpc_build_class(PyObject *name, PyObject *bases, PyObject *ns, PyObject *kwds);
PyObject *fpc_prepare(PyObject *name, PyObject *bases, PyObject *kwds);
PyObject *fpc_format_value(PyObject *v, int conv, PyObject *spec);
PyObject *fpc_make_generator(FpcGenBody body, PyObject *fn, PyObject **slots, int nslots, const char *name);
PyObject *fpc_yield(FpyGen *g, PyObject *v);
PyObject *fpc_yield_from(FpyGen *g, PyObject *iterable);
PyObject *fpc_await(FpyGen *g, PyObject *awaitable);
PyObject *fpc_await_steal(FpyGen *g, PyObject *awaitable);
PyObject *fpc_make_coroutine(FpcGenBody body, PyObject *fn, PyObject **slots, int nslots, const char *name);
#endif
