# AGENTS.md — working on fpython

fpython is a strictly typed, natively compiled Python 4.x-style language. Source files use the
`.fpy` extension and are syntactically valid Python 3.14. The `fpython` toolchain compiles them to
C and then to static native binaries suitable for `FROM scratch` Docker images.

`CLAUDE.md` is a symlink to this file. Edit `AGENTS.md` only.

**Requirements live in [`REQUIREMENTS.md`](REQUIREMENTS.md).** Read it first, and update its
status markers as work lands.

## Layout

| Path | What lives there |
| --- | --- |
| `src/lexer.rs`, `src/parser.rs`, `src/ast.rs` | Front end. Parses the full Python 3.14 grammar. |
| `src/types.rs` | The fpython type model (`Ty`) and its mapping to C storage kinds. |
| `src/compiler/` | Type checker and C code generator, which run as a single pass with monomorphized generics. |
| `src/fmt.rs` | `fpython fmt`, the canonical formatter. |
| `src/doc.rs` | `fpython doc`, the API documentation generator (HTML/Markdown from docstrings). |
| `src/convert.rs` | `fpython convert`, which turns Python into fpython (inferring type annotations). |
| `src/driver.rs`, `src/main.rs` | The CLI: build/run/check/fmt/convert/init/test. Also invokes the C compiler. |
| `runtime/` | The C runtime, embedded into the compiler binary via `include_str!`. Files: `gc.c` (GC), `str.c`, `containers.c`, `core.c` (exceptions, protocols, dynamic `Any` ops), `gen.c` (generators as coroutines). |
| `stdlib/` | fpython standard library modules written in fpython itself, using `@extern` C bindings. |
| `tests/` | End-to-end tests: `tests/run/*.fpy` paired with `*.out` holding the expected stdout. |
| `bench/` | Benchmarks against Go, Rust and CPython. |
| `docker/` | Toolchain image and a `FROM scratch` example. |

## Design rules

- Every value is a 64-bit word. `int` is `int64_t` with overflow checking, `float` is `double`,
  `None` is `NULL`, and heap objects begin with an `FpyType*`. Containers store the element storage
  kind (`FK_INT/FLOAT/BOOL/OBJ`).
- Typing is strict: every function signature needs annotations, and locals are inferred. `Any` is
  allowed only when written explicitly. Python modules imported without annotations are compiled
  in dynamic (`Any`) mode. A module that cannot be compiled natively falls back to embedded
  CPython (`--python-fallback`).
- Do not add Rust crate dependencies without a strong reason. The toolchain is std-only.
- The generated C relies on GNU extensions (statement expressions, `__builtin_*_overflow`), and both
  clang and gcc are supported.
- License: PSF-2.0, the same license as CPython. New files are covered by `LICENSE`.

## Commands

```sh
cargo build --release                 # build the toolchain
./target/release/fpython run x.fpy    # compile + run
./target/release/fpython fmt .        # format all .fpy files
./target/release/fpython convert src/ # python -> fpython
cargo test                            # unit tests + tests/run end-to-end suite
```

When you change the runtime or codegen, run `cargo test`. Every `tests/run/*.fpy` must reproduce
its `.out` file exactly. Where the semantics match, also compare against `python3`.

## Quality requirements (set by the project owner)

- **Unit-test coverage ≥ 95%** for the Rust toolchain, measured with `cargo llvm-cov`. Every module
  carries `#[cfg(test)]` unit tests.
- **End-to-end tests:**
  - `tests/run/*.fpy`: compile, run, and compare against golden output.
  - A Docker multi-stage build whose final stage is `FROM scratch`. The test builds the image and
    runs the container.
  - Interop scripts that import real Python 3.14 libraries (`fastapi`, `fastmcp`, `requests`,
    `httpx`, `boto3`, `unittest`, ...). Pure-Python code is compiled natively, and whatever cannot be
    compiled falls back to embedded CPython.
- **Benchmarks:** `bench/` compares execution time of fpython against CPython 3.14, with Go and Rust
  included as reference points. Results are written to `bench/RESULTS.md`.
