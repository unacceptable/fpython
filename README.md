# fpython

**fpython** is a strictly typed, natively compiled language based on Python, meant as a "Python
4.x". You write ordinary-looking Python (`.fpy` files are valid Python 3.14 syntax), and
`fpython` type-checks it and compiles it to a native executable that runs at Go/Rust-like speed.
The same binary can still use the Python 3.14 library ecosystem.

```python
from dataclasses import dataclass


@dataclass
class Point:
    x: float
    y: float

    def norm2(self) -> float:
        return self.x * self.x + self.y * self.y


def farthest(points: list[Point]) -> Point:
    return max(points, key=lambda p: p.norm2())


print(farthest([Point(1, 2), Point(3, 4), Point(-5, 0)]))
```

```console
$ fpython run demo.fpy
Point(x=3.0, y=4.0)
$ fpython build --release --static demo.fpy -o demo   # one static binary, ready for FROM scratch
```

## Why

| | CPython 3.14 | fpython |
|---|---|---|
| Typing | optional hints | **strict**: compile-time errors; `Any` is an explicit opt-in |
| Execution | bytecode interpreter | **native code**, via C, with monomorphized generics and unboxed ints/floats |
| Speed (benchmark geomean) | 1× | **9.1× faster than Python**, 0.83× the speed of Go (see [Performance](#performance)) |
| Deployment | interpreter + sources | **one static binary**; a `FROM scratch` image is about 1 MB |
| Libraries | everything | everything: pure-Python dependencies are compiled to native code, and the rest runs on embedded CPython 3.14 |

## Performance

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="bench/benchmarks-dark.png">
  <img alt="Benchmark chart: wall-clock time per benchmark for fpython, Python 3.14, Go and Rust, and the overall speed-up over Python" src="bench/benchmarks.png">
</picture>

Across six workloads, fpython is **9.1x faster than Python 3.14** (geometric mean) and runs at
0.83x the speed of Go and 0.64x the speed of Rust. The Python versions
(`bench/python/*.py`) are written the way a Python developer would write them: no annotations, and
fast builtins such as `Counter`, `bytearray` slicing, `sum()` and tuples. Go and Rust are idiomatic
ports, included as native reference points.

| benchmark | fpython | Python 3.14 | Go | Rust | fpython vs Python |
|---|---:|---:|---:|---:|---:|
| fib (recursion) | 0.031s | 0.777s | 0.034s | 0.026s | **25.1x faster** |
| nbody (float/objects) | 0.067s | 4.908s | 0.107s | 0.040s | **73.3x faster** |
| spectral norm | 0.036s | 3.132s | 0.038s | 0.025s | **86.3x faster** |
| sieve (list[bool]) | 0.132s | 0.040s | 0.031s | 0.020s | 3.3x slower |
| words (str/dict) | 0.161s | 0.466s | 0.107s | 0.124s | **2.9x faster** |
| binary trees (GC) | 0.759s | 3.114s | 0.892s | 1.247s | **4.1x faster** |

Times are the best of 3 runs on an Apple Silicon Mac (wall clock, including process start-up);
every implementation must print identical output. Python wins `sieve`: there `bytearray` slice
assignment runs entirely in C, while fpython still stores `list[bool]` as 8-byte words.

Reproduce with `cargo build --release && python3 bench/run.py`, which writes
[bench/RESULTS.md](bench/RESULTS.md), `bench/results.json` and the charts (redraw them with
`python3 bench/chart.py`; requires matplotlib).

## Install

```sh
cargo build --release          # produces target/release/fpython (std-only Rust, no crates)
export PATH=$PWD/target/release:$PATH
```

You need a C compiler (`cc`, `clang` or `gcc`). Using Python libraries additionally requires
Python 3.14 (`python3`).

## The `fpython` command

| command | what it does |
|---|---|
| `fpython run app.fpy [args]` | compile and run |
| `fpython build app.fpy [-o out] [--release] [--static] [--target T] [--emit-c]` | native executable |
| `fpython check app.fpy` | type-check only |
| `fpython fmt [--check] [paths]` | canonical formatter (Black-like, idempotent, keeps comments) |
| `fpython lint [paths]` | fast linter: 23 rules, `# noqa` support |
| `fpython doc [paths] [--format html\|md] [--check]` | API docs from docstrings: HTML site with search, or Markdown |
| `fpython convert file.py\|dir [--trace]` | **2to3-style** Python → fpython converter that infers type annotations |
| `fpython test [paths]` | runs `test_*` functions in `test_*.fpy`, compiled natively |
| `fpython init name` | new project (`fpython.toml`, src/, tests/, Dockerfile) |
| `fpython bundle exe rootfs` | rootfs for `FROM scratch` images that embed CPython |

## Language in one page

fpython is Python with a stricter contract:

* **Every function signature is annotated** (`def f(x: int) -> str:`). Local variables are
  inferred from their first assignment and keep that type.
* `int` is a 64-bit integer, and overflow raises `OverflowError` rather than wrapping. `float` is
  an IEEE double. `str` is immutable UTF-8 text.
* Built-in types: `int float bool str bytes list[T] dict[K, V] set[T] tuple[A, B]
  tuple[T, ...] T | None`, unions, `Callable[[A], R]`, `Iterator[T]`, `Generator[Y, S, R]`,
  type aliases (`type X = ...`, `X: TypeAlias = ...`, `X = list[int]`), and `Any`.
* Classes, single inheritance, `@dataclass` (with `order=`/`frozen=`), `@property`,
  `@staticmethod`/`@classmethod`, `Enum`, `Protocol`, and operator overloading (`__add__`, `__eq__`,
  `__lt__`, `__len__`, `__getitem__`, `__iter__`, ...).
* Generics use PEP 695 syntax (`def first[T](xs: list[T]) -> T`, `class Stack[T]`), and each
  instantiation is monomorphized like Rust/C++.
* Narrowing works on `is None`, `isinstance`, truthiness and early returns.
* Generators (with `send`/`throw`/`close`), closures (`nonlocal`), lambdas, comprehensions,
  `match`, `with`, and `try/except/else/finally` with exception chaining (`raise ... from`,
  `__cause__`/`__context__`) are supported.
* `async def`/`await` interoperate with asyncio (through the CPython bridge).
* `cast(T, x)` converts `Any` values to typed ones. Dynamic values are checked at runtime.

Errors read like a compiler's:

```console
$ fpython check bad.fpy
bad.fpy:3:5: error: type mismatch: expected int, got str
 3 |     return name
   |     ^^^^^^^^^^^
```

## Using Python libraries

Any module that is not part of fpython's own stdlib is imported from Python 3.14:

```python
import requests                      # pure Python: compiled to native code automatically
from fastapi import FastAPI          # FastAPI/pydantic: C extensions run on embedded CPython

app = FastAPI()


@app.get("/square/{n}")
def square(n: int) -> dict[str, int]:   # a native function, introspectable by FastAPI
    return {"square": n * n}
```

* Python values are `Any` in fpython. Annotate them (`x: int = resp.json()["n"]`) to get checked
  conversions into native types.
* Native functions handed to Python behave like normal Python functions, with real signatures,
  so FastAPI and fastmcp work. Native classes can subclass Python classes (`unittest.TestCase`,
  `pydantic.BaseModel`, `http.server.BaseHTTPRequestHandler`). `async def` coroutines are driven by
  asyncio.
* **Native compilation of pure-Python dependencies** is the default for directly imported
  third-party and local modules. `FPY_NATIVE_PY=deep` compiles entire packages, and
  `FPY_NATIVE_PY=none` turns this off. The compiled code drives the CPython object model directly,
  as Cython/Nuitka do, including `async def`/`await`/`async with`/`async for`. Anything that
  can't be compiled faithfully (async generators, `match`, `locals()`/`eval`) falls back to
  normal import.
  `FPY_VERBOSE=1` reports which path each module took.

Verified end to end (see `tests/py/`): **fastapi, fastmcp, requests, httpx, boto3, unittest**.

## Docker: `FROM scratch`

Pure-native programs produce a fully static binary:

```dockerfile
FROM fpython:latest AS build              # docker build -f docker/Dockerfile -t fpython:latest .
COPY app.fpy .
RUN fpython build --release --static app.fpy -o /out/app

FROM scratch
COPY --from=build /out/app /app
ENTRYPOINT ["/app"]
```

[`examples/hello-docker`](examples/hello-docker) builds a 941 kB image.

Programs that use Python libraries are packaged together with the embedded interpreter:

```dockerfile
FROM fpython-py:latest AS build           # docker build -f docker/Dockerfile.python -t fpython-py .
RUN pip install fastapi uvicorn
COPY app.fpy .
RUN fpython build --release app.fpy -o /out/app && fpython bundle /out/app /rootfs

FROM scratch
COPY --from=build /rootfs /
ENTRYPOINT ["/app"]
```

[`examples/fastapi-docker`](examples/fastapi-docker) serves FastAPI with uvicorn from `FROM scratch`.

## How it works

```
.fpy ──lexer/parser (full Python 3.14 grammar)──► AST ──type checker + C codegen──► C ──cc──► native
                                                         │
                         runtime (C): GC, str/list/dict, generators (stackful coroutines),
                         exceptions (setjmp), CPython bridge (pybridge.c), Python→C (pyc.c)
```

* **Values:** each value is one 64-bit word. Heap objects carry a type pointer, and containers
  record their element storage kind.
* **Memory:** a precise-heap / conservative-stack mark-and-sweep GC, thread-aware when CPython
  threads call back into native code.
* **Generics:** monomorphized. Bounds are checked per instantiation.
* The toolchain is written in Rust with no dependencies (≈20k lines). The runtime is C.

## Project docs

* [REQUIREMENTS.md](REQUIREMENTS.md): project requirements and their status
* [AGENTS.md](AGENTS.md) (also `CLAUDE.md`): contributor and agent guide
* [bench/RESULTS.md](bench/RESULTS.md): latest benchmark results

## License

fpython is released under the same license as Python: the **PSF License Agreement (PSF-2.0)**.
See [LICENSE](LICENSE).
