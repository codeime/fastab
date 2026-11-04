#!/usr/bin/env python3
"""Compare what builds of the engine, and allocator settings, leave in the heap.

    scripts/engine-memory-ab.py bundle/specs-ir before=/tmp/engine-memory head=target/dist/examples/engine-memory
    scripts/engine-memory-ab.py --rounds 3 bundle/specs-ir a=./a "a-empty=./a,no-history" "a-efficient=./a,MallocSpaceEfficient=1"

Each NAME=PATH is `examples/engine-memory` from some commit, run in a fresh
process from the current directory. After the path, `no-history` drops the
history file and `KEY=VALUE` sets an environment variable. Builds run
interleaved, so a runner that slows down hits them alike. Numbers are medians in
KiB: footprint is phys_footprint when the engine was built, while busy, after
its caches retired, right after the pressure relief the desktop runs when quiet,
and 20 s later. `in use` and `frag` split the malloc zones at that last point,
`released` is what the relief call reported, and `spread` is the last footprint's
range across rounds. The steps table shows the footprint of the first build as
each command of the first burst finishes.
"""

import argparse
import os
import statistics
import subprocess


def run_once(path, specs, no_history, environment):
    argv = [path, specs, *(["--no-history"] if no_history else [])]
    output = subprocess.run(argv, capture_output=True, text=True, check=True, env=dict(os.environ, **environment)).stdout
    phases = {}
    for line in output.splitlines():
        fields = line.split()
        if fields[0] == "released":
            phases["released"] = int(fields[1])
        else:
            phases[fields[0]] = tuple(int(field) for field in fields[1:])
    return phases


def parse_build(spec, parser):
    name, sep, rest = spec.partition("=")
    path, *options = rest.split(",")
    if not sep or not os.access(path, os.X_OK):
        parser.error(f"not NAME=EXECUTABLE[,OPTION...]: {spec}")
    environment = dict(option.split("=", 1) for option in options if "=" in option)
    return name, os.path.abspath(path), "no-history" in options, environment


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("specs")
    parser.add_argument("builds", nargs="+", metavar="NAME=PATH[,no-history][,KEY=VALUE]")
    parser.add_argument("--rounds", type=int, default=3)
    args = parser.parse_args()
    specs = os.path.abspath(args.specs)
    builds = [parse_build(spec, parser) for spec in args.builds]

    samples = {build[0]: [] for build in builds}
    for _ in range(args.rounds):
        for name, path, no_history, environment in builds:
            try:
                samples[name].append(run_once(path, specs, no_history, environment))
            except (subprocess.CalledProcessError, ValueError, IndexError) as error:
                print(f"{name}: {error}")

    def kib(values):
        return int(statistics.median(values) / 1024)

    width = max(len(build[0]) for build in builds)
    print(
        f"{'build':<{width}} {'built':>7} {'busy':>7} {'idle':>7} {'relieved':>8} {'settled':>8}"
        f" {'in use':>7} {'frag':>7} {'released':>8} {'spread':>7}  (KiB, median of {args.rounds})"
    )
    for name, _, _, _ in builds:
        runs = samples[name]
        if not runs:
            print(f"{name:<{width}} no samples")
            continue
        footprint = lambda phase: kib(run[phase][0] for run in runs)
        settled = [run["settled"] for run in runs]
        print(
            f"{name:<{width}} {footprint('built'):7} {footprint('busy'):7} {footprint('idle'):7}"
            f" {footprint('relieved'):8} {footprint('settled'):8}"
            f" {kib(item[1] for item in settled):7} {kib(item[2] - item[1] for item in settled):7}"
            f" {kib(run['released'] for run in runs):8}"
            f" {int((max(item[0] for item in settled) - min(item[0] for item in settled)) / 1024):7}"
        )

    first = samples[builds[0][0]]
    if first:
        print(f"steps of {builds[0][0]} (footprint, in use, KiB):")
        for phase in (key for key in first[0] if key.startswith("step:")):
            print(f"  {phase[5:]:<16} {kib(run[phase][0] for run in first):7} {kib(run[phase][1] for run in first):7}")


if __name__ == "__main__":
    main()
