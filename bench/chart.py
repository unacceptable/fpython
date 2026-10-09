#!/usr/bin/env python3
"""Draw the benchmark bar charts from bench/results.json (written by bench/run.py).

Writes bench/benchmarks.png (light) and bench/benchmarks-dark.png (dark) for the README.
Requires matplotlib: `python3 -m pip install matplotlib`.
"""
import json
import os
import statistics
import sys

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))

# (key in results.json, legend label, light color, dark color)
SERIES = [
    ("python", "Python {v}", "#3572a5", "#5b9bd5"),
    ("fpython", "fpython", "#d62f6b", "#ff5c93"),
    ("go", "Go", "#00a29c", "#2ec4b6"),
    ("rust", "Rust", "#c97d2e", "#e8a25c"),
]

THEMES = {
    "light": {"bg": "#ffffff", "fg": "#1f2328", "muted": "#59636e", "grid": "#d8dee4"},
    "dark": {"bg": "#0d1117", "fg": "#e6edf3", "muted": "#9198a1", "grid": "#30363d"},
}


def draw(data, theme, path):
    t = THEMES[theme]
    benches = data["benchmarks"]
    times = data["times"]
    v = data.get("python_version", "3")
    series = [s for s in SERIES if all(times.get(b, {}).get(s[0]) for b in benches)]
    plt.rcParams.update({"font.family": "sans-serif", "font.size": 11})
    fig, (ax, ax2) = plt.subplots(1, 2, figsize=(14, 5.6), gridspec_kw={"width_ratios": [3.2, 1]}, facecolor=t["bg"])
    for a in (ax, ax2):
        a.set_facecolor(t["bg"])
        for side in ("top", "right"):
            a.spines[side].set_visible(False)
        for side in ("left", "bottom"):
            a.spines[side].set_color(t["grid"])
        a.tick_params(colors=t["fg"])

    # left: wall-clock time per benchmark (log scale), grouped bars
    n = len(series)
    width = 0.8 / n
    for i, (key, label, light, dark) in enumerate(series):
        xs = [j + (i - (n - 1) / 2) * width for j in range(len(benches))]
        ys = [times[b][key] for b in benches]
        ax.bar(xs, ys, width * 0.92, label=label.format(v=v), color=light if theme == "light" else dark, zorder=3)
    ax.set_yscale("log")
    ax.set_xticks(range(len(benches)), benches)
    ax.set_ylabel("seconds (log scale, lower is better)", color=t["muted"])
    ax.grid(axis="y", color=t["grid"], linewidth=0.8, zorder=0)
    ax.set_title(f"Wall-clock time per benchmark  (labels: fpython vs Python {v})", color=t["fg"], fontsize=13, loc="left", pad=12)
    # fpython's speed-up over regular Python above each group
    for j, b in enumerate(benches):
        f, p = times[b]["fpython"], times[b].get("python")
        if p:
            txt = f"{p / f:.1f}x" if p >= f else f"{f / p:.1f}x slower"
            top = max(times[b][s[0]] for s in series)
            ax.annotate(txt, (j, top * 1.45), ha="center", color=t["muted"], fontsize=9.5)
    ax.set_ylim(top=max(times[b][s[0]] for b in benches for s in series) * 3)
    leg = ax.legend(loc="upper left", ncol=n, frameon=False, fontsize=9.5, bbox_to_anchor=(0, -0.08))
    for txt in leg.get_texts():
        txt.set_color(t["fg"])

    # right: geometric-mean speed-up over regular Python
    base = statistics.geometric_mean([times[b]["python"] for b in benches])
    keys = [s for s in series if s[0] != "python"]
    speed = [base / statistics.geometric_mean([times[b][s[0]] for b in benches]) for s in keys]
    labels = [s[1].format(v=v) for s in keys]
    colors = [s[2] if theme == "light" else s[3] for s in keys]
    ax2.barh(range(len(keys)), speed, color=colors, zorder=3)
    ax2.set_yticks(range(len(keys)), labels)
    ax2.invert_yaxis()
    ax2.axvline(1, color=t["muted"], linewidth=1, linestyle="--", zorder=2)
    for i, s in enumerate(speed):
        ax2.annotate(f"{s:.1f}x" if s >= 1 else f"{s:.2f}x", (s, i), xytext=(4, 0), textcoords="offset points", va="center", color=t["fg"], fontsize=10)
    ax2.set_xlim(0, max(speed) * 1.25)
    ax2.set_xlabel(f"x faster than Python {v} (geomean)\ndashed line = Python", color=t["muted"])
    ax2.grid(axis="x", color=t["grid"], linewidth=0.8, zorder=0)
    ax2.set_title("Overall", color=t["fg"], fontsize=13, loc="left", pad=12)

    fig.tight_layout()
    fig.savefig(path, dpi=150, facecolor=t["bg"])
    plt.close(fig)


def main():
    src = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "results.json")
    with open(src) as fh:
        data = json.load(fh)
    draw(data, "light", os.path.join(HERE, "benchmarks.png"))
    draw(data, "dark", os.path.join(HERE, "benchmarks-dark.png"))
    print("wrote bench/benchmarks.png and bench/benchmarks-dark.png")


if __name__ == "__main__":
    main()
