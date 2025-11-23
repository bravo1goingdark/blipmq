#!/usr/bin/env python3
"""
Generate comparison plots from bench_compare results.

Usage:
    python plot.py --results ./results/results.csv --outdir ./results
"""
import argparse
import csv
from collections import defaultdict
from pathlib import Path

import matplotlib.pyplot as plt


def load_results(path: Path):
    results = []
    with path.open("r", newline="") as f:
        reader = csv.DictReader(f)
        for row in reader:
            # Coerce numeric fields.
            for key in (
                "message_size",
                "publishers",
                "subscribers",
                "total_messages",
            ):
                row[key] = int(row[key])
            for key in (
                "throughput_mps",
                "latency_p50_us",
                "latency_p95_us",
                "latency_p99_us",
                "cpu_percent",
                "memory_bytes",
            ):
                row[key] = float(row[key])
            results.append(row)
    return results


def aggregate_by_size(results, metric):
    """Return dict[(broker_label, message_size)] = average(metric)."""
    buckets = defaultdict(list)
    for row in results:
        label = f"{row['broker']}:{row['qos']}"
        buckets[(label, row["message_size"])].append(float(row[metric]))
    aggregated = {}
    for key, values in buckets.items():
        aggregated[key] = sum(values) / len(values)
    return aggregated


def plot_metric(results, metric, ylabel, out_file):
    aggregated = aggregate_by_size(results, metric)
    labels = sorted({k[0] for k in aggregated})
    sizes = sorted({k[1] for k in aggregated})

    x_positions = range(len(sizes))
    width = 0.18
    fig, ax = plt.subplots(figsize=(10, 6))

    for idx, label in enumerate(labels):
        offsets = [x + (idx - len(labels) / 2) * width for x in x_positions]
        values = [aggregated.get((label, size), 0.0) for size in sizes]
        ax.bar(offsets, values, width=width, label=label)

    ax.set_xticks(list(x_positions))
    ax.set_xticklabels([f"{s}B" for s in sizes])
    ax.set_ylabel(ylabel)
    ax.set_xlabel("Message size")
    ax.set_title(f"{metric} by broker and message size")
    ax.legend()
    ax.grid(True, linestyle="--", alpha=0.4)

    fig.tight_layout()
    fig.savefig(out_file)
    plt.close(fig)
    print(f"[plot] wrote {out_file}")


def main():
    parser = argparse.ArgumentParser(description="Plot bench_compare results.")
    parser.add_argument("--results", required=True, help="Path to results.csv")
    parser.add_argument(
        "--outdir",
        default=".",
        help="Directory to write PNGs (throughput.png, latency_p95.png)",
    )
    args = parser.parse_args()

    results_path = Path(args.results)
    outdir = Path(args.outdir)
    outdir.mkdir(parents=True, exist_ok=True)

    results = load_results(results_path)
    plot_metric(results, "throughput_mps", "Msgs/sec", outdir / "throughput.png")
    plot_metric(results, "latency_p95_us", "p95 latency (us)", outdir / "latency_p95.png")
    plot_metric(results, "latency_p99_us", "p99 latency (us)", outdir / "latency_p99.png")


if __name__ == "__main__":
    main()
