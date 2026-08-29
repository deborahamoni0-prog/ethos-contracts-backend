#!/usr/bin/env python3
"""Compare trigger_release benchmark output against the tracked baseline.

Runs `cargo test --package ttl-vault bench_trigger_release -- --nocapture`,
parses the `trigger_release(n=N) -> cpu=... mem=...` lines it prints, and
compares each against contracts/ttl_vault/benches/baseline.json within the
configured tolerance. Exits non-zero (fails CI) if any benchmark regresses
beyond tolerance; prints a warning table either way.

Usage:
    scripts/compare_bench_baseline.py [--update-baseline] [--warn-only]
"""
import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
BASELINE_PATH = REPO_ROOT / "contracts" / "ttl_vault" / "benches" / "baseline.json"

LINE_RE = re.compile(
    r"trigger_release\(n=\s*(?P<n>\d+)\s*\)\s*.*?cpu=\s*(?P<cpu>\d+)\s*mem=\s*(?P<mem>\d+)"
)

NAME_BY_N = {
    1: "bench_trigger_release_1_beneficiary",
    5: "bench_trigger_release_5_beneficiaries",
    10: "bench_trigger_release_10_beneficiaries",
    20: "bench_trigger_release_20_beneficiaries",
    50: "bench_trigger_release_50_beneficiaries",
}


def run_benchmarks() -> str:
    result = subprocess.run(
        [
            "cargo",
            "test",
            "--package",
            "ttl-vault",
            "bench_trigger_release",
            "--",
            "--nocapture",
        ],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    print(result.stdout)
    print(result.stderr, file=sys.stderr)
    if result.returncode != 0:
        print("cargo test failed to run benchmarks", file=sys.stderr)
        sys.exit(result.returncode)
    return result.stdout + result.stderr


def parse_results(output: str) -> dict:
    results = {}
    for match in LINE_RE.finditer(output):
        n = int(match.group("n"))
        name = NAME_BY_N.get(n)
        if name is None:
            continue
        results[name] = {"n": n, "cpu": int(match.group("cpu")), "mem": int(match.group("mem"))}
    return results


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--update-baseline",
        action="store_true",
        help="Overwrite baseline.json with freshly measured results instead of comparing.",
    )
    parser.add_argument(
        "--warn-only",
        action="store_true",
        help="Print regressions but always exit 0 (useful for a non-blocking CI warning step).",
    )
    args = parser.parse_args()

    baseline_doc = json.loads(BASELINE_PATH.read_text())
    tolerance_percent = baseline_doc.get("tolerance_percent", 15)
    output = run_benchmarks()
    measured = parse_results(output)

    if not measured:
        print("No benchmark output matched the expected format; nothing to compare.", file=sys.stderr)
        return 1

    if args.update_baseline:
        for name, values in measured.items():
            baseline_doc["benchmarks"].setdefault(name, {})
            baseline_doc["benchmarks"][name].update(values)
        BASELINE_PATH.write_text(json.dumps(baseline_doc, indent=2) + "\n")
        print(f"Updated {BASELINE_PATH} with {len(measured)} measurements.")
        return 0

    regressions = []
    print(f"{'benchmark':<40} {'baseline cpu':>14} {'measured cpu':>14} {'delta %':>10}")
    for name, baseline in baseline_doc["benchmarks"].items():
        current = measured.get(name)
        if current is None:
            print(f"{name:<40} {'(not run)':>14}")
            continue
        base_cpu = baseline["cpu"]
        delta_percent = ((current["cpu"] - base_cpu) / base_cpu) * 100 if base_cpu else 0.0
        print(f"{name:<40} {base_cpu:>14} {current['cpu']:>14} {delta_percent:>9.1f}%")
        if delta_percent > tolerance_percent:
            regressions.append((name, base_cpu, current["cpu"], delta_percent))

    if regressions:
        print("\nPerformance regressions beyond tolerance "
              f"({tolerance_percent}%):", file=sys.stderr)
        for name, base_cpu, cur_cpu, delta_percent in regressions:
            print(
                f"  {name}: {base_cpu} -> {cur_cpu} cpu ({delta_percent:+.1f}%)",
                file=sys.stderr,
            )
        if args.warn_only:
            print("\n--warn-only set: not failing the build.", file=sys.stderr)
            return 0
        return 1

    print("\nNo regressions beyond tolerance.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
