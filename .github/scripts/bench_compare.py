#!/usr/bin/env python3
"""Compare criterion bench results against a saved baseline; fail on
>THRESHOLD regression (CLAUDE.md v1.0.0 addendum / NFR-1: "CI fails on >15%
regression vs the committed baseline").

Criterion writes `target/criterion/<group>/<bench>/new/estimates.json` for
every bench on every run (no special flag). This script reads the *current*
run's `new/estimates.json` files and compares each bench's median point
estimate (nanoseconds — criterion's native unit for `estimates.json`) against
the matching baseline file. A bench is a regression when `current_median >
baseline_median * (1 + THRESHOLD)` — i.e. it got slower.

Baseline format mirrors criterion's tree, one `estimates.json` per bench:
    <baseline-dir>/<group>/<bench>/estimates.json

Benches present in the current run but absent from the baseline are reported
as "establishing" (no comparison, never a failure) — this is the first run
after a cache eviction, where the baseline is simply (re)seeded for next time.

Why compare median time (not throughput): for these benches time and throughput
are exact inverses (criterion derives throughput from elapsed time and the
declared `Throughput`), so a time increase is a throughput decrease. Median is
robust to the outlier samples criterion occasionally reports.

Why per-runner-class caching (not a git-committed baseline): a committed
baseline generated on one machine and compared on another is dominated by
hardware differences, not code changes — a 15% gate would be meaningless. The
nightly workflow restores the previous run's baseline from an actions cache
keyed by runner OS, so current-vs-baseline is the same runner class (modulo the
15% threshold absorbing pool variance). See `docs/performance.md`.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path


def _median(path: Path) -> float:
    """Read a criterion `estimates.json` and return its median point estimate
    (nanoseconds — criterion's native unit). The caller checks for KeyError."""
    with path.open("r", encoding="utf-8") as fh:
        return float(json.load(fh)["median"]["point_estimate"])


def _walk_current(current_root: Path):
    """Yield (bench_key, estimates.json path) for every `new/estimates.json`
    under `current_root` (the `target/criterion` dir). `bench_key` is the
    `<group>/<bench>` path (e.g. `blob_read/1MiB`), used to match the baseline."""
    for est in current_root.rglob("new/estimates.json"):
        # <current_root>/<group>/<bench>/new/estimates.json -> <group>/<bench>
        rel = est.parent.parent.relative_to(current_root)
        yield rel.as_posix(), est


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--current", required=True, help="target/criterion dir")
    parser.add_argument("--baseline", required=True, help="saved baseline dir")
    parser.add_argument(
        "--threshold",
        type=float,
        default=0.15,
        help="regression fraction that fails the gate (default 0.15 = 15 percent)",
    )
    args = parser.parse_args()

    current_root = Path(args.current)
    baseline_root = Path(args.baseline)

    if not current_root.is_dir():
        print(f"error: current criterion dir not found: {current_root}", file=sys.stderr)
        return 1

    regressions: list[tuple[str, float, float, float]] = []
    compared = 0
    established = 0

    for key, cur_est in sorted(_walk_current(current_root)):
        base_est = baseline_root / key / "estimates.json"
        if not base_est.is_file():
            print(f"  - {key}: (establishing baseline; no comparison)")
            established += 1
            continue
        try:
            base_median = _median(base_est)
            cur_median = _median(cur_est)
        except (OSError, KeyError, ValueError) as exc:
            print(f"  ! {key}: could not read estimates ({exc})", file=sys.stderr)
            continue
        ratio = cur_median / base_median if base_median > 0 else float("inf")
        pct = (ratio - 1.0) * 100.0
        compared += 1
        marker = "  ok"
        if ratio > 1.0 + args.threshold:
            marker = "  REGRESSION"
            regressions.append((key, base_median, cur_median, pct))
        elif ratio < 1.0 - args.threshold:
            marker = "  improvement"
        print(
            f"  - {key}: baseline={base_median / 1e3:9.1f}us "
            f"current={cur_median / 1e3:9.1f}us  {pct:+6.1f}% {marker}"
        )

    print(
        f"\nCompared {compared} bench(es), established {established}; "
        f"{len(regressions)} regression(s) over {args.threshold * 100:.0f}%."
    )
    if regressions:
        print("\nFAIL: regression(s) detected:")
        for key, base, cur, pct in regressions:
            print(f"  - {key}: {base / 1e3:.1f}us -> {cur / 1e3:.1f}us ({pct:+.1f}%)")
        return 2
    print("PASS: no regression over the threshold.")
    return 0


if __name__ == "__main__":
    sys.exit(main())