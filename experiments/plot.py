#!/usr/bin/env python3
"""Plots of the published experiment results, with seaborn.

Reads docs/experiments/<mode>/<benchmark>/{compare.json,report.json} and
writes PNGs, inlined by docs/experiments.md, to docs/experiments/plots/:

  errors.png        signed error of the replay against the original in
                    units of each field's tolerance, every field,
                    benchmark and mode
  within.png        fields within tolerance per benchmark and mode; the
                    crowd mode's copies of a profile are one bar with
                    their range
  convergence.png   seconds to the last knob move, per benchmark and mode
  trajectories-<mode>.png
                    each mode's fits: IPC, MPKI and dTLB rate against
                    their targets over the first seconds
  disturbed.png     the disturbed mode: IPC, MPKI and dTLB rate of each
                    live fit over its hundred seconds, the neighbour's
                    span shaded, with the fields within per phase

Usage: experiments/plot.py [docs/experiments [png|svg]]
"""

import json
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
import pandas as pd  # noqa: E402
import seaborn as sns  # noqa: E402

MODES = ["user", "kernel", "resctrl", "noisy", "crowd", "perfmon"]
BENCHES = ["schbench", "sysbench", "taobench", "stress-ng", "fio"]
PHASES = ["alone", "beside", "after"]


def base_name(benchmark: str) -> str:
    """The profile behind a directory name: `taobench-2` is `taobench`."""
    head, _, tail = benchmark.rpartition("-")
    return head if head and tail.isdigit() else benchmark


def bench_order(benchmark: str):
    """Sort key: the set's order, copies after one another."""
    base = base_name(benchmark)
    return (BENCHES.index(base) if base in BENCHES else len(BENCHES), benchmark)
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


# What "within tolerance" means, for the figures that count it.
TOLERANCE_NOTE = (
    f"within tolerance: {RATIO:.0%} of the target, or the field's absolute floor if that is wider: "
    "busy cores 0.05, burstiness 0.02, IPC 0.02, L3 MPKI 0.05, DRAM 100 MB/s, "
    "dTLB 0.02 and iTLB 0.01 per thousand instructions, memory 8 MiB"
)


def load(root: Path):
    rows, fits = [], []
    for mode in MODES:
        if not (root / mode).is_dir():
            continue
        benches = sorted(
            (d.name for d in (root / mode).iterdir() if (d / "compare.json").exists()),
            key=bench_order,
        )
        for bench in benches:
            d = root / mode / bench
            compare = json.loads((d / "compare.json").read_text())
            report = json.loads((d / "report.json").read_text())
            for r in compare["rows"]:
                if r.get("actual") is None:
                    continue
                tol = tolerance(r["key"], r["expected"])
                rows.append(
                    dict(
                        mode=mode,
                        benchmark=bench,
                        field=r["key"],
                        # The error in units of the field's tolerance: the
                        # band is one either way, whatever the field.
                        error=(r["actual"] - r["expected"]) / tol if tol > 0 else 0.0,
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


FORMAT = "png"


def save(fig, out: Path, name: str):
    fig.savefig(out / f"{name}.{FORMAT}", dpi=160)
    plt.close(fig)


def errors(df: pd.DataFrame, out: Path):
    keys = [k for k, _ in FIELDS]
    labels = dict(FIELDS)
    df = df[df.field.isin(keys)].copy()
    df["field"] = pd.Categorical(df.field.map(labels), [labels[k] for k in keys])
    df["row"] = df["mode"] + " / " + df["benchmark"]
    order = sorted(
        set(df.row),
        key=lambda r: (MODES.index(r.split(" / ")[0]), bench_order(r.split(" / ")[1])),
    )
    pivot = df.pivot_table(index="row", columns="field", values="error", observed=False)
    pivot = pivot.reindex(order)
    within = df.pivot_table(index="row", columns="field", values="within", observed=False)
    within = within.reindex(order)
    shown = pivot.clip(-4, 4)
    annot = pivot.map(lambda e: "" if pd.isna(e) else (f"{e:+.1f}" if abs(e) < 10 else f"{e:+.0f}"))
    fig, ax = plt.subplots(figsize=(11, 0.42 * len(order) + 1.8))
    sns.heatmap(
        shown,
        cmap="vlag",
        center=0,
        vmin=-4,
        vmax=4,
        annot=annot,
        fmt="",
        annot_kws=dict(size=7),
        linewidths=0.5,
        cbar_kws=dict(
            label="replay less original, in tolerances of the field (±1 is the band; colour clipped at ±4)"
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
    ax.set_title("Replay error by field, in tolerances; a frame marks a field outside its band")
    fig.text(0.5, 0.005, TOLERANCE_NOTE, ha="center", va="bottom", fontsize=7, wrap=True)
    fig.tight_layout(rect=(0, 0.03, 1, 1))
    save(fig, out, "errors")


def within(df: pd.DataFrame, out: Path):
    counts = (
        df.groupby(["mode", "benchmark"], observed=False)
        .agg(within=("within", "sum"), measured=("within", "size"))
        .reset_index()
    )
    # Copies of one profile, the crowd mode's, share a bar: its height
    # is their mean and the line their range.
    counts["profile"] = counts["benchmark"].map(base_name)
    counts["mode"] = pd.Categorical(counts["mode"], MODES)
    counts["profile"] = pd.Categorical(counts["profile"], BENCHES)
    counts = counts.sort_values(["profile", "mode"])
    fig, ax = plt.subplots(figsize=(9, 4.2))
    sns.barplot(
        counts,
        x="profile",
        y="measured",
        hue="mode",
        palette=["#e6e6e6"] * len(MODES),
        errorbar=None,
        ax=ax,
        legend=False,
    )
    sns.barplot(counts, x="profile", y="within", hue="mode", errorbar=("pi", 100), ax=ax)
    ax.set_ylabel("fields within tolerance (of fields measured, grey)")
    ax.set_xlabel("")
    ax.legend(title="mode", ncols=6, loc="lower center", bbox_to_anchor=(0.5, 1.02))
    fig.text(0.5, 0.005, TOLERANCE_NOTE, ha="center", va="bottom", fontsize=7, wrap=True)
    fig.tight_layout(rect=(0, 0.05, 1, 1))
    save(fig, out, "within")


def convergence(fits: pd.DataFrame, out: Path):
    fits = fits.copy()
    fits["profile"] = fits["benchmark"].map(base_name)
    fits["mode"] = pd.Categorical(fits["mode"], MODES)
    fits["profile"] = pd.Categorical(fits["profile"], BENCHES)
    fits["outcome"] = fits.apply(
        lambda r: "converged" if r.converged else ("held closest" if r.held else "not converged"),
        axis=1,
    )
    fig, ax = plt.subplots(figsize=(9, 4.2))
    sns.stripplot(
        fits,
        x="profile",
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
            (BENCHES.index(r.profile), r.last_move_s),
            ha="center",
            va="center",
            fontsize=8,
            color="black",
        )
    ax.set_ylabel("seconds to the last knob move (× not converged)")
    ax.set_xlabel("")
    ax.set_ylim(bottom=0)
    ax.legend(title="mode", ncols=6, loc="lower center", bbox_to_anchor=(0.5, 1.02))
    fig.tight_layout()
    save(fig, out, "convergence")


def disturbed(root: Path, out: Path):
    """Each live fit's IPC, MPKI and dTLB rate over the run, the
    neighbour's span shaded, the tolerance band drawn, and the fields
    within per recorded phase in the title."""
    mode = root / "disturbed"
    if not mode.is_dir():
        return
    benches = sorted(
        (d.name for d in mode.iterdir() if (d / "phases.json").exists()), key=bench_order
    )
    if not benches:
        return
    fig, axes = plt.subplots(
        len(TRAJECTORY_FIELDS),
        len(benches),
        figsize=(3.4 * len(benches) + 1, 2.2 * len(TRAJECTORY_FIELDS) + 1.0),
        sharex=True,
        squeeze=False,
    )
    for j, bench in enumerate(benches):
        d = mode / bench
        report = json.loads((d / "report.json").read_text())
        phases = json.loads((d / "phases.json").read_text())
        targets = {t["key"]: t for t in report["targets"]}
        rows = [
            dict(t=w["end_s"], field=v["key"], value=v["measured"])
            for w in report["windows"]
            for v in w["values"]
            if v["measured"] is not None
        ]
        series_all = pd.DataFrame(rows)
        within = []
        for ph in PHASES:
            c = json.loads((d / f"compare-{ph}.json").read_text())["rows"]
            measured = [r for r in c if r.get("actual") is not None]
            within.append(f"{ph} {sum(r['within'] for r in measured)}/{len(measured)}")
        for i, (key, label) in enumerate(TRAJECTORY_FIELDS):
            ax = axes[i][j]
            series = series_all[series_all.field == key]
            if not series.empty:
                sns.lineplot(series, x="t", y="value", ax=ax, drawstyle="steps-post")
            ax.axvspan(
                phases["neighbour"]["start_s"],
                phases["neighbour"]["stop_s"],
                color="tab:orange",
                alpha=0.15,
                lw=0,
            )
            for lo, hi in phases["phases"].values():
                ax.axvspan(lo, hi, color="tab:grey", alpha=0.08, lw=0)
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
                ax.set_title(f"{bench}: " + ", ".join(within), fontsize=9)
    fig.suptitle(
        "disturbed mode: a live fit with a neighbour over the orange span; grey spans are the recorded phases, the title their fields within"
    )
    fig.tight_layout()
    save(fig, out, "disturbed")


def trajectories(fits: pd.DataFrame, out: Path, mode="user", seconds=4.0):
    sub = fits[fits["mode"] == mode]
    if sub.empty:
        return
    benches = sorted(set(sub.benchmark), key=bench_order)
    fig, axes = plt.subplots(
        len(TRAJECTORY_FIELDS),
        len(benches),
        figsize=(2.6 * len(benches) + 1, 2.2 * len(TRAJECTORY_FIELDS) + 0.8),
        sharex=True,
        squeeze=False,
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
    save(fig, out, f"trajectories-{mode}")


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
    # The crowd's twelve copies would make a figure too wide to read;
    # its within bars carry their range.
    for mode in MODES:
        if mode != "crowd":
            trajectories(fits, out, mode)
    disturbed(root, out)
    print(f"plots written to {out}")


if __name__ == "__main__":
    main()
