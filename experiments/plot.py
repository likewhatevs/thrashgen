#!/usr/bin/env python3
"""Plots of the published experiment results, with seaborn.

Reads docs/experiments/<mode>/<benchmark>/{compare.json,report.json} and
writes SVGs to docs/experiments/plots/:

  errors.svg        signed relative error of the replay against the
                    original, every field, every benchmark, every mode
  within.svg        fields within tolerance per benchmark and mode
  convergence.svg   seconds to the last knob move, per benchmark and mode
  trajectories.svg  the user-mode fits: IPC, MPKI and dTLB rate against
                    their targets over the first seconds

Usage: experiments/plot.py [docs/experiments [svg|png]]
"""

import json
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
import pandas as pd  # noqa: E402
import seaborn as sns  # noqa: E402

MODES = ["user", "kernel", "resctrl", "noisy", "perfmon"]
BENCHES = ["schbench", "sysbench", "taobench", "stress-ng", "fio"]
FIELDS = [
    ("cpu_usage_cores", "busy cores"),
    ("cpu_usage_cv_1s", "burstiness"),
    ("ipc", "IPC"),
    ("l3_mpki", "L3 MPKI"),
    ("dram_bytes_per_sec", "DRAM B/s"),
    ("dtlb_pki", "dTLB /k"),
    ("itlb_pki", "iTLB /k"),
    ("anon_bytes", "anon"),
    ("pagecache_bytes", "page cache"),
    ("shmem_bytes", "shmem"),
]
TRAJECTORY_FIELDS = [("ipc", "IPC"), ("l3_mpki", "L3 MPKI"), ("dtlb_pki", "dTLB /k")]
# The absolute floors of the tolerance, as `fit::absolute_tolerance`
# has them: within a floor of the target a field is met whatever the
# ratio says, so errors are shown against the larger of the two.
FLOORS = {
    "cpu_usage_cores": 0.05,
    "cpu_usage_cv_1s": 0.02,
    "ipc": 0.02,
    "l3_mpki": 0.05,
    "dram_bytes_per_sec": 1.0e8,
    "dtlb_pki": 0.02,
    "itlb_pki": 0.01,
    "memory_current_bytes": 8.0 * 1024 * 1024,
    "anon_bytes": 8.0 * 1024 * 1024,
    "pagecache_bytes": 8.0 * 1024 * 1024,
    "shmem_bytes": 8.0 * 1024 * 1024,
}
RATIO = 0.05


def tolerance(key: str, target: float) -> float:
    return max(RATIO * abs(target), FLOORS.get(key, 0.0))


def load(root: Path):
    rows, fits = [], []
    for mode in MODES:
        for bench in BENCHES:
            d = root / mode / bench
            if not (d / "compare.json").exists():
                continue
            compare = json.loads((d / "compare.json").read_text())
            report = json.loads((d / "report.json").read_text())
            for r in compare["rows"]:
                if r.get("actual") is None:
                    continue
                scale = max(abs(r["expected"]), FLOORS.get(r["key"], 0.0))
                rows.append(
                    dict(
                        mode=mode,
                        benchmark=bench,
                        field=r["key"],
                        error=(r["actual"] - r["expected"]) / scale if scale > 0 else 0.0,
                        within=r["within"],
                    )
                )
            windows = report["windows"]
            moves = [
                w["end_s"]
                for a, w in zip(windows, windows[1:])
                if w["knobs"] != a["knobs"]
            ]
            fits.append(
                dict(
                    mode=mode,
                    benchmark=bench,
                    passes=report["passes"],
                    converged=report["converged"],
                    last_move_s=moves[-1] if moves else 0.0,
                    held=any("closest" in n for n in report["notes"]),
                    report=report,
                )
            )
    return pd.DataFrame(rows), pd.DataFrame(fits)


FORMAT = "svg"


def save(fig, out: Path, name: str):
    fig.savefig(out / f"{name}.{FORMAT}", dpi=160)
    plt.close(fig)


def errors(df: pd.DataFrame, out: Path):
    keys = [k for k, _ in FIELDS]
    labels = dict(FIELDS)
    df = df[df.field.isin(keys)].copy()
    df["field"] = pd.Categorical(df.field.map(labels), [labels[k] for k in keys])
    df["row"] = df["mode"] + " / " + df["benchmark"]
    order = [f"{m} / {b}" for m in MODES for b in BENCHES]
    order = [r for r in order if r in set(df.row)]
    pivot = df.pivot_table(index="row", columns="field", values="error", observed=False)
    pivot = pivot.reindex(order)
    within = df.pivot_table(index="row", columns="field", values="within", observed=False)
    within = within.reindex(order)
    shown = (pivot * 100).clip(-100, 100)
    annot = pivot.map(lambda e: "" if pd.isna(e) else f"{e * 100:+.0f}%")
    fig, ax = plt.subplots(figsize=(11, 0.42 * len(order) + 1.6))
    sns.heatmap(
        shown,
        cmap="vlag",
        center=0,
        vmin=-100,
        vmax=100,
        annot=annot,
        fmt="",
        linewidths=0.5,
        cbar_kws=dict(
            label="replay less original, percent of the target or of its absolute floor, whichever is larger (clipped at ±100)"
        ),
        ax=ax,
    )
    for (i, row), (j, col) in (
        (ri, ci) for ri in enumerate(pivot.index) for ci in enumerate(pivot.columns)
    ):
        ok = within.loc[row, col]
        if ok is False or ok == 0:
            ax.add_patch(
                plt.Rectangle((j, i), 1, 1, fill=False, edgecolor="black", lw=1.6)
            )
    ax.set_xlabel("")
    ax.set_ylabel("")
    ax.set_title("Replay error by field; a frame marks a field outside tolerance")
    fig.tight_layout()
    save(fig, out, "errors")


def within(df: pd.DataFrame, out: Path):
    counts = (
        df.groupby(["mode", "benchmark"], observed=False)
        .agg(within=("within", "sum"), measured=("within", "size"))
        .reset_index()
    )
    counts["mode"] = pd.Categorical(counts["mode"], MODES)
    counts["benchmark"] = pd.Categorical(counts["benchmark"], BENCHES)
    counts = counts.sort_values(["benchmark", "mode"])
    fig, ax = plt.subplots(figsize=(9, 4.2))
    sns.barplot(
        counts,
        x="benchmark",
        y="measured",
        hue="mode",
        palette=["#e6e6e6"] * len(MODES),
        ax=ax,
        legend=False,
    )
    sns.barplot(counts, x="benchmark", y="within", hue="mode", ax=ax)
    ax.set_ylabel("fields within tolerance (of fields measured, grey)")
    ax.set_xlabel("")
    ax.legend(title="mode", ncols=5, loc="lower center", bbox_to_anchor=(0.5, 1.02))
    fig.tight_layout()
    save(fig, out, "within")


def convergence(fits: pd.DataFrame, out: Path):
    fits = fits.copy()
    fits["mode"] = pd.Categorical(fits["mode"], MODES)
    fits["benchmark"] = pd.Categorical(fits["benchmark"], BENCHES)
    fits["outcome"] = fits.apply(
        lambda r: "converged" if r.converged else ("held closest" if r.held else "not converged"),
        axis=1,
    )
    fig, ax = plt.subplots(figsize=(9, 4.2))
    sns.stripplot(
        fits,
        x="benchmark",
        y="last_move_s",
        hue="mode",
        dodge=True,
        size=9,
        marker="o",
        ax=ax,
    )
    for _, r in fits[fits.outcome != "converged"].iterrows():
        ax.annotate(
            "×",
            (BENCHES.index(r.benchmark), r.last_move_s),
            ha="center",
            va="center",
            fontsize=8,
            color="black",
        )
    ax.set_ylabel("seconds to the last knob move (× not converged)")
    ax.set_xlabel("")
    ax.set_ylim(bottom=0)
    ax.legend(title="mode", ncols=5, loc="lower center", bbox_to_anchor=(0.5, 1.02))
    fig.tight_layout()
    save(fig, out, "convergence")


def trajectories(fits: pd.DataFrame, out: Path, mode="user", seconds=4.0):
    sub = fits[fits["mode"] == mode]
    if sub.empty:
        return
    benches = [b for b in BENCHES if b in set(sub.benchmark)]
    fig, axes = plt.subplots(
        len(TRAJECTORY_FIELDS),
        len(benches),
        figsize=(2.6 * len(benches) + 1, 2.2 * len(TRAJECTORY_FIELDS) + 0.8),
        sharex=True,
    )
    for j, bench in enumerate(benches):
        report = sub[sub.benchmark == bench].iloc[0].report
        targets = {t["key"]: t for t in report["targets"]}
        rows = []
        for w in report["windows"]:
            if w["end_s"] > seconds:
                break
            for v in w["values"]:
                if v["measured"] is not None:
                    rows.append(dict(t=w["end_s"], field=v["key"], value=v["measured"]))
        d = pd.DataFrame(rows)
        for i, (key, label) in enumerate(TRAJECTORY_FIELDS):
            ax = axes[i][j]
            series = d[d.field == key]
            if not series.empty:
                sns.lineplot(series, x="t", y="value", ax=ax, drawstyle="steps-post")
            log = key in ("l3_mpki", "dtlb_pki") and not series.empty and series.value.max() > 0
            if key in targets:
                target = targets[key]["target"]
                tol = tolerance(key, target)
                low = target - tol
                if log:
                    low = max(low, min(series.value.min(), target) / 3)
                ax.axhspan(low, target + tol, color="tab:green", alpha=0.2, lw=0)
                ax.axhline(target, color="tab:green", lw=1)
            if log:
                ax.set_yscale("log")
            ax.set_ylabel(label if j == 0 else "")
            ax.set_xlabel("seconds" if i == len(TRAJECTORY_FIELDS) - 1 else "")
            if i == 0:
                ax.set_title(bench)
    fig.suptitle(
        f"{mode} mode: the fit's first {seconds:.0f} seconds; the band is the tolerance, 5 percent or the field's floor"
    )
    fig.tight_layout()
    save(fig, out, "trajectories")


def main():
    global FORMAT
    root = Path(sys.argv[1] if len(sys.argv) > 1 else "docs/experiments")
    if len(sys.argv) > 2:
        FORMAT = sys.argv[2]
    out = root / "plots"
    out.mkdir(parents=True, exist_ok=True)
    sns.set_theme(style="whitegrid", context="paper")
    df, fits = load(root)
    if df.empty:
        sys.exit(f"no results under {root}")
    errors(df, out)
    within(df, out)
    convergence(fits, out)
    trajectories(fits, out)
    print(f"plots written to {out}")


if __name__ == "__main__":
    main()
