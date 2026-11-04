#!/usr/bin/env python3
"""Compare what builds of the engine leave in the heap.

    scripts/engine-memory-ab.py bundle/specs-ir before=/tmp/engine-memory head=target/dist/examples/engine-memory
    scripts/engine-memory-ab.py --rounds 3 bundle/specs-ir a=./a "a-empty=./a,no-history"

Each build is `examples/engine-memory` from some commit, run in a fresh process
from the current directory. Builds run interleaved, so a runner that slows down
hits them alike. Numbers are medians in KiB: footprint is phys_footprint when
the engine was built, while busy, after its caches retired, and after the same
pressure relief the desktop runs when quiet. `in use` and `frag` split the malloc
zones at that last point, and `spread` is that footprint's range across rounds.
"""

import argparse
import os
import statistics
import subprocess


def run_once(path, specs, no_history):
    argv = [path, specs, *(["--no-history"] if no_history else [])]
    output = subprocess.run(argv, capture_output=True, text=True, check=True).stdout
    phases = {}
    for line in output.splitlines():
        phase, footprint, in_use, allocated = line.split()
        phases[phase] = (int(footprint), int(in_use), int(allocated))
    return phases


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("specs")
    parser.add_argument("builds", nargs="+", metavar="NAME=PATH[,no-history]")
    parser.add_argument("--rounds", type=int, default=5)
    args = parser.parse_args()
    specs = os.path.abspath(args.specs)

    builds = []
    for spec in args.builds:
        name, sep, rest = spec.partition("=")
        path, _, flag = rest.partition(",")
        if not sep or not os.access(path, os.X_OK) or flag not in ("", "no-history"):
            parser.error(f"not NAME=EXECUTABLE[,no-history]: {spec}")
        builds.append((name, os.path.abspath(path), flag == "no-history"))

    samples = {name: [] for name, _, _ in builds}
    for _ in range(args.rounds):
        for name, path, no_history in builds:
            try:
                samples[name].append(run_once(path, specs, no_history))
            except (subprocess.CalledProcessError, ValueError) as error:
                print(f"{name}: {error}")

    width = max(len(name) for name, _, _ in builds)
    print(
        f"{'build':<{width}} {'built':>7} {'busy':>7} {'idle':>7} {'relieved':>8} {'in use':>7} {'frag':>7} {'spread':>7}"
        f"  (KiB, median of {args.rounds})"
    )
    for name, _, _ in builds:
        runs = samples[name]
        if not runs:
            print(f"{name:<{width}} no samples")
            continue

        def footprint(phase):
            return int(statistics.median(run[phase][0] for run in runs) / 1024)

        relieved = [run["relieved"] for run in runs]
        in_use = int(statistics.median(item[1] for item in relieved) / 1024)
        fragmented = int(statistics.median(item[2] - item[1] for item in relieved) / 1024)
        spread = int((max(item[0] for item in relieved) - min(item[0] for item in relieved)) / 1024)
        print(
            f"{name:<{width}} {footprint('built'):7} {footprint('busy'):7} {footprint('idle'):7}"
            f" {footprint('relieved'):8} {in_use:7} {fragmented:7} {spread:7}"
        )


if __name__ == "__main__":
    main()
