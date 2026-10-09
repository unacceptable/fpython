#!/usr/bin/env python3
"""Benchmark fpython against Python 3.14, Go and Rust; writes bench/RESULTS.md.

bench/*.fpy are the fpython programs. bench/python/ holds regular, idiomatic Python versions of
the same algorithms (no annotations, using fast builtins such as Counter, bytearray slicing and
sum()); go/ and rust/ hold idiomatic Go and Rust. Outputs of all implementations are compared
before timing.
"""
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
FPY = os.environ.get("FPY", os.path.join(ROOT, "target", "release", "fpython"))
BENCHES = ["fib", "nbody", "spectral", "sieve", "words", "trees"]
RUNS = int(os.environ.get("BENCH_RUNS", "3"))
OUT = os.path.join(HERE, "build")


def sh(cmd, **kw):
    return subprocess.run(cmd, check=True, capture_output=True, text=True, **kw)


def build_all():
    os.makedirs(OUT, exist_ok=True)
    exes = {}
    for b in BENCHES:
        exes[(b, "fpython")] = [os.path.join(OUT, f"fpy_{b}")]
        sh([FPY, "build", "--release", os.path.join(HERE, f"{b}.fpy"), "-o", exes[(b, "fpython")][0]])
        exes[(b, "python")] = [sys.executable, os.path.join(HERE, "python", f"{b}.py")]
        if shutil.which("go"):
            exes[(b, "go")] = [os.path.join(OUT, f"go_{b}")]
            sh(["go", "build", "-o", exes[(b, "go")][0], os.path.join(HERE, "go", f"{b}.go")])
        if shutil.which("rustc"):
            exes[(b, "rust")] = [os.path.join(OUT, f"rs_{b}")]
            sh(["rustc", "-O", "-o", exes[(b, "rust")][0], os.path.join(HERE, "rust", f"{b}.rs")])
    return exes


def time_cmd(cmd):
    subprocess.run(cmd, check=True, capture_output=True)  # warm-up (also skips first-launch checks)
    times = []
    out = None
    for _ in range(RUNS):
        t0 = time.perf_counter()
        r = subprocess.run(cmd, check=True, capture_output=True, text=True)
        times.append(time.perf_counter() - t0)
        out = r.stdout
    return min(times), statistics.median(times), out


def main():
    exes = build_all()
    langs = ["fpython", "python", "go", "rust"]
    results = {}
    for b in BENCHES:
        ref = None
        for lang in langs:
            if (b, lang) not in exes:
                continue
            best, med, out = time_cmd(exes[(b, lang)])
            if ref is None:
                ref = out
            elif out != ref:
                raise SystemExit(f"output mismatch for {b}/{lang}:\n{out}\nvs\n{ref}")
            results[(b, lang)] = best
            print(f"{b:9s} {lang:8s} {best:8.3f}s (median {med:.3f}s)", flush=True)
    pyver = ".".join(platform.python_version_tuple()[:2])
    lines = [
        "# fpython benchmarks",
        "",
        f"Machine: {platform.platform()} / {platform.machine()}, {os.cpu_count()} CPUs. "
        f"Best of {RUNS} runs (after one warm-up), wall-clock seconds, including process start-up.",
        "",
        f"- **Python {pyver}**: regular, idiomatic Python (`bench/python/*.py`: no annotations, fast builtins).",
        "- **Go** / **Rust**: idiomatic ports (`bench/go`, `bench/rust`) as native reference points.",
        "",
        f"| benchmark | fpython | Python {pyver} | Go | Rust | fpython vs Python | fpython vs Go | fpython vs Rust |",
        "|---|---:|---:|---:|---:|---:|---:|---:|",
    ]

    def fmt(x):
        return f"{x:.3f}" if x is not None else "-"

    def ratio(a, base):
        if a is None or base is None:
            return "-"
        return f"{base / a:.1f}x faster" if base > a else f"{a / base:.2f}x slower"

    for b in BENCHES:
        t = {lang: results.get((b, lang)) for lang in langs}
        lines.append(
            f"| {b} | {fmt(t['fpython'])} | {fmt(t['python'])} | {fmt(t['go'])} | {fmt(t['rust'])} "
            f"| {ratio(t['fpython'], t['python'])} | {ratio(t['fpython'], t['go'])} | {ratio(t['fpython'], t['rust'])} |"
        )
    geo = {}
    for lang in langs:
        xs = [results[(b, lang)] for b in BENCHES if (b, lang) in results]
        if len(xs) == len(BENCHES):
            geo[lang] = statistics.geometric_mean(xs)
    names = {"fpython": "fpython", "python": f"Python {pyver}", "go": "Go", "rust": "Rust"}
    lines += ["", "Geometric mean (seconds): " + ", ".join(f"{names[k]} {v:.3f}" for k, v in geo.items())]
    if "fpython" in geo and "python" in geo:
        lines.append(
            f"\nfpython is **{geo['python'] / geo['fpython']:.1f}x faster than Python {pyver}** (geometric mean)."
        )
    if "go" in geo and "fpython" in geo:
        lines.append(f"fpython runs at **{geo['go'] / geo['fpython']:.2f}x the speed of Go** and **{geo.get('rust', 0) / geo['fpython']:.2f}x the speed of Rust** (geometric mean; >1 means faster).")
    lines += [
        "",
        "Workloads: `fib` (recursion), `nbody` (float math on objects), `spectral` (nested float loops),",
        "`sieve` (list-of-bool scanning), `words` (string building, split, dict counting, sorting),",
        "`trees` (allocation-heavy binary trees, exercises the GC).",
        "",
        "![benchmark chart](benchmarks.png)",
        "",
        "Reproduce: `cargo build --release && python3 bench/run.py` (the chart needs matplotlib; redraw it",
        "from `bench/results.json` with `python3 bench/chart.py`).",
    ]
    with open(os.path.join(HERE, "RESULTS.md"), "w") as fh:
        fh.write("\n".join(lines) + "\n")
    print("\n".join(lines))
    # raw numbers for bench/chart.py
    data = {
        "python_version": pyver,
        "machine": f"{platform.platform()} / {platform.machine()}",
        "runs": RUNS,
        "benchmarks": BENCHES,
        "times": {b: {lang: results[(b, lang)] for lang in langs if (b, lang) in results} for b in BENCHES},
    }
    with open(os.path.join(HERE, "results.json"), "w") as fh:
        json.dump(data, fh, indent=2)
        fh.write("\n")
    try:
        import chart  # needs matplotlib

        chart.main()
    except ImportError:
        print("matplotlib is not installed; skipping bench/benchmarks.png (pip install matplotlib)")


if __name__ == "__main__":
    main()
