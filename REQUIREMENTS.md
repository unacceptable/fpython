# fpython — Project Requirements

This file is the source of truth for what fpython must do. It is kept up to date so requirements
survive context resets. Every requirement comes from the project owner. Status markers:
`[ ]` not started · `[~]` in progress · `[x]` done and verified.

## Language

- [~] **R1 — Python 4.x-style language.** fpython is a new programming language based on Python.
  Source files use the `.fpy` extension and are syntactically valid Python 3.14.
- [~] **R2 — Strict typing.** Typing is strict by default: every function signature is annotated,
  locals are inferred, and type errors are compile errors. Users may opt into dynamic typing
  explicitly with `Any`.
- [~] **R3 — Speed comparable to Go/Rust.** fpython compiles to native code (Rust toolchain → C →
  native, with monomorphized generics and unboxed ints/floats).

## Deployment

- [x] **R4 — `FROM scratch` Docker images.** The toolchain produces fully static binaries, so the
  final stage of a multi-stage Docker build can be `FROM scratch`. An official toolchain image and
  an example Dockerfile are provided.
  - `docker/Dockerfile` (Alpine/musl toolchain) and `examples/hello-docker` (final stage `FROM
    scratch`) are verified: the image is 941 kB and runs on linux/arm64.

## Python interop

- [x] **R5 — Use existing Python 3.14 libraries.** Pure-Python libraries are compiled natively
  first (dynamic/`Any` mode). Anything that cannot be compiled falls back to embedded CPython 3.14,
  which must still work in a static `FROM scratch` image.
  - Libraries that must work, verified by end-to-end tests: `fastapi`, `fastmcp`, `requests`,
    `httpx`, `boto3`, `unittest`, and others.
  - [x] CPython fallback bridge (`runtime/pybridge.c`). It handles imports, dynamic ops,
    kwargs, exceptions in both directions, native functions exposed as real Python functions with
    signatures, Python-derived classes with native methods, `async def`/`await` interop with
    asyncio, thread-safe GC registration, and `with` handling with `__exit__` suppression.
  - [x] Verified on the host: `tests/py/{unittest_basic,http_clients,boto3_basic,fastapi_app,fastmcp_server}.fpy`
    all pass (requests, httpx, boto3 with Stubber, FastAPI with TestClient, fastmcp with an async
    Client, unittest).
  - [x] CPython fallback inside a `FROM scratch` image. `docker/Dockerfile.python` builds the
    toolchain on python:3.14-alpine, and `fpython bundle` assembles the executable, its shared
    libraries, the Python home and CA certificates into a rootfs. `examples/fastapi-docker` is
    verified: FastAPI + uvicorn serve HTTP from `FROM scratch` (106 MB).
  - [x] Native compilation of pure-Python modules before falling back. `src/pycomp.rs` compiles
    `.py` modules to C against the CPython object model (Cython/Nuitka-style), and
    `runtime/pyc.c` loads them through a meta-path finder. Supported: functions (as real
    `function` objects with signatures), classes, closures, generators, `try`/`with`,
    `async def`/`await`/`async with`/`async for` (coroutines are driven by asyncio and anyio).
    Modules with unsupported constructs (async generators, `match`, frame-dependent
    `locals`/`vars`/`eval`) fall back to bytecode import, reported per module with `FPY_VERBOSE=1`.
    - Verified on 2026-10-09 with `FPY_NATIVE_PY=deep FPY_NO_FALLBACK=1`: every `tests/py`
      program matches its golden output, with fastapi using 113 native and 7 fallback modules,
      fastmcp using 221 native and 28 fallback, boto3 40 native and 0 fallback, and
      httpx/requests 28 native and 8 fallback. `tests/e2e.rs::native_python_module_compilation`
      guards this.
  - [ ] Optional: async generators and `match` in the Python→C compiler (the remaining 43 deep-mode
    fallbacks).

## CLI: `fpython`

- [x] **R6 — `fpython fmt <file|dir>`** formats `.fpy` files automatically. It is canonical and
  idempotent, and supports `--check`, `--diff` and `-`.
  - Verified on 2026-10-09 by `fpython fmt-verify` on 32,794 Python files (the CPython 3.14 stdlib
    plus site-packages): 100% preserve the AST, are idempotent, and keep every comment.
- [x] **R7 — `fpython convert <script|dir>`** works like `2to3`: it converts Python to fpython by
  inferring and inserting type annotations, rewriting unsupported constructs, and reporting what
  needs manual attention.
  - Static inference uses defaults, call sites, return expressions, `.append`/`d[k]=` usage and
    attribute types. `--trace` adds runtime type collection. `%`/`.format` are rewritten to
    f-strings. Output is formatted and type-checked, and remaining issues are reported.
  - Verified with `tests/convert/inventory.py`: it converts with zero `Any`, and the converted program
    produces output identical to CPython.
- [x] **R8 — `fpython lint <file|dir>`** is a linter for `.fpy` files: it reports style and
  correctness issues without compiling. It covers 23 rule codes (F/T/B/N/W/E/I), supports
  `# noqa` and `# fpy: ignore[...]` suppressions, and was crash-tested on the CPython stdlib.
- [x] **R16 — `fpython doc [paths]`** generates API documentation from module, class and
  function docstrings. It writes an HTML site (an index with search, a page per module, signatures
  as `fpython fmt` prints them, links from signatures to documented classes, dark mode) or Markdown
  (`--format md`, `--stdout`). It respects `__all__` and `_private` names (`--private` shows them),
  understands Google- and NumPy-style sections, `>>>` examples and code blocks, and
  `--check` lists public items without a docstring (exit 1). In a project it defaults to `src/` →
  `build/docs`.
  - Verified on 2026-10-09: unit tests in `src/doc.rs` and `tests/cli.rs::doc_generation`; run on
    `stdlib/` (14 modules).
- [~] Other subcommands: `build`, `run`, `check`, `test`, `init`, plus `fpython.toml` project files.

## Licensing and repository

- [x] **R9 — License.** The project uses the same OSS license as Python: PSF License Agreement
  (PSF-2.0).
- [x] **R10 — Agent docs.** `AGENTS.md` exists, and `CLAUDE.md` is a symlink to `AGENTS.md`.
- [x] **R11 — This file.** Requirements are tracked in `REQUIREMENTS.md`.

## Quality

- [x] **R12 — Coverage.** Unit-test coverage is at least 95% for the toolchain, measured with
  `cargo llvm-cov --release --summary-only` (unit tests + `tests/cli.rs` + `tests/e2e.rs`; the
  e2e/CLI tests drive the instrumented binary).
  - 2026-10-09: 95.24% lines (17,316 lines, 824 missed), up from 72.4%. Test inventory: 61 unit
    tests, 33 golden programs (incl. `regressions.fpy`, `regress_imports.fpy`,
    `regress_generators.fpy`), 257 compile-error tests, 9 native Python-module programs (direct +
    deep mode), the CLI suite and the interop suite. Keep it at or above 95% when adding code.
- [x] **R13 — End-to-end tests** (`tests/e2e.rs`, all passing on 2026-10-09; the Docker test runs with `FPY_DOCKER_TESTS=1`):
  - golden-output tests for `.fpy` programs;
  - a Docker multi-stage build whose final stage is `FROM scratch`, with the container run as part
    of the test;
  - scripts importing real Python libraries (see R5).
- [x] **R14 — Benchmarks.** Execution time of CPython 3.14 vs fpython is compared, with Go and Rust
  as reference points. Results are written to `bench/RESULTS.md`.
  - `python3 bench/run.py` covers six workloads, each as fpython, as regular idiomatic Python
    (`bench/python/*.py`, added 2026-10-09 at the owner's request), and as Go and Rust; all outputs
    are compared. `bench/chart.py` (matplotlib) draws `bench/benchmarks.png` (+ dark) for the README.
    Latest (2026-10-09): **9.1x faster than Python** (geometric mean), 0.83x Go, 0.64x Rust.
    Known gap: `sieve` (`list[bool]` uses 8-byte elements; Python's bytearray slicing is faster).

## Infrastructure and one-off tasks

- [x] **R15 — Domain.** Register `fpython.org` if it is available, using `AWS_PROFILE=aztek-org`.
  It was available, and registration was submitted on 2026-10-09 (Route 53 operation
  `4667d103-8565-40b2-ba04-936cf42bfd89`, $16/yr, auto-renew on, privacy on, contacts copied from
  `aztek.io`). Confirmed SUCCESSFUL on 2026-10-09.

## Design decisions (made by the project owner)

- Interop: compile pure-Python natively first, and use embedded CPython only as a fallback.
- Toolchain: written in Rust, emitting C, with static musl builds.
- Typing: strict, with explicit `Any` allowed.
- CLI name: `fpython`. File extension: `.fpy`.

## Bug backlog (found by the coverage push, 2026-10-09)

Each fix gets a regression case in a golden program or error test.

- [x] B1 bytes slicing segfaulted (`fpy_bytes_new(NULL, n)` memcpy from NULL).
- [x] B2 `fpython run` printed nothing when the program died from a signal (output now flushed, signal named).
- [x] B3 function-local `import m` of a native module that was not imported at top level: C compile error.
- [x] B4 `list(b"hi")` / `sorted(bytes)` crash (bytes passed to `fpy_list_from_iter`).
- [x] B5 mapping pattern `**rest` keeps the matched keys.
- [x] B6 nested function values ignore `*args` and defaults ("function value expects N arguments").
- [x] B7 `bytes * int`, `x in bytes`, `x in range(...)` rejected.
- [x] B8 f-strings: `{m}` ignores `__format__`; `!a` doesn't escape; `ascii()` missing; `{x = }` loses spaces.
- [x] B9 exception texts: `str(KeyError("k"))` should be `'k'`; `repr(AssertionError())`; unpack error message wording.
- [x] B10 `self.items = list(items)` with `*items: int` param: cannot infer attribute type.
- [x] B11 class variables vs fields: `x: T = v` is an instance field unless assigned via the class; assigning a real class variable through an instance is a compile error.
- [x] B12 generator `.send()`, `e.args`, `e.__cause__` unsupported in native code.
- [x] B13 pattern errors use `Span::default()` (reported at 0:0).
- [x] B14 no narrowing in value-returning `and` (`name and name.upper()` with `str | None`).
- [x] B15 implicit type aliases (`A = list[int]`) and `X: TypeAlias = ...` rejected.
- [x] B16 `tuple[int, ...]` treated as `list[int]`, so tuples are rejected.
- [x] B17 relative imports in a package `__init__` resolve one level too high.
- [x] B18 function-local `from m import VAR` (non-function) fails.
- [x] B19 dotted module paths in annotations (`pkg.mod.Box[float]`) rejected.
- [x] B20 calling a `type[Base]` value always constructs `Base` (wrong output).
- [x] B21 `cast(Iterator[int], any_value)` yields boxed pointers (wrong output).
- [x] B22 class-as-`Iterable[T]` assignability depends on compile order.
- [x] B23 `int | None` not assignable to `int | str | None` (fixed); `bytearray` stays annotation-only.
- [x] B24 pycomp: `{*tuple}` in a set display calls `update(*tuple)`.
- [x] B25 pycomp: implicit exception `__context__` never set.
- [x] B26 pycomp: class body inside a function can't see the function's locals.
- [x] B27 pycomp: `__qualname__` not nested (`A.m`, `f.<locals>.g`).
- [x] B28 pycomp: async comprehensions compiled as sync loops (must compile or be rejected).
- [x] B29 printing a Python `frozenset` from fpython shows `{1}`.
- [x] B30 a global first created inside a function via `global` is "not defined" at module level.
- [x] B31 `float.hex()` drops trailing zero digits (`0x1p+0` vs `0x1.0000000000000p+0`).
- [x] B32 `next()` on an `Any`-typed native iterator raises AttributeError.
- [x] B33 passing a lambda to an `Any`-typed native function segfaults.
- [x] B34 `str(b"abc")` decodes instead of giving `"b'abc'"`; `bytes(b)` rejected; `sum([], start=3)` rejected.
