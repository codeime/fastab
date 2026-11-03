#!/usr/bin/env python3
"""What Fastab's shell integration adds to starting a shell.

    scripts/shell-startup-cost.py target/dist/ftab
    scripts/shell-startup-cost.py --rounds 30 ~/.local/bin/ftab

Times `zsh -i -c exit` with an empty .zshrc and with the two `ftab init` lines
the installer writes: once as if the desktop app were running
(Q_FORCE_FIGTERM_LAUNCH) and once with it down, where init stands down. Also
times the single `ftab` calls on their own; `ftab _ pre-cmd` without a session
exits right after starting, so it is the launch cost a prompt pays. Stdout is
not a terminal, so `init zsh pre` never execs fterm and no desktop app is
needed. Cases run interleaved; numbers are wall-clock medians and p90 in ms.
"""

import argparse
import math
import os
import statistics
import subprocess
import tempfile
import time


def run(argv, env):
    started = time.perf_counter()
    subprocess.run(argv, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return (time.perf_counter() - started) * 1000


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("ftab", help="the ftab binary to measure")
    parser.add_argument("--rounds", type=int, default=20)
    args = parser.parse_args()
    ftab = os.path.abspath(args.ftab)

    base = dict(os.environ)
    base["PATH"] = os.path.dirname(ftab) + os.pathsep + base.get("PATH", "")
    for name in ("QTERM_SESSION_ID", "Q_TERM", "Q_SHELL", "SHOULD_QTERM_LAUNCH", "PROCESS_LAUNCHED_BY_Q"):
        base.pop(name, None)
    app_up = dict(base, Q_FORCE_FIGTERM_LAUNCH="1")
    app_down = dict(base)
    app_down.pop("Q_FORCE_FIGTERM_LAUNCH", None)

    with tempfile.TemporaryDirectory() as root:
        bare = os.path.join(root, "bare")
        integrated = os.path.join(root, "integrated")
        os.mkdir(bare)
        os.mkdir(integrated)
        open(os.path.join(bare, ".zshrc"), "w").close()
        with open(os.path.join(integrated, ".zshrc"), "w") as rc:
            rc.write(f"eval \"$('{ftab}' init zsh pre --rcfile zshrc)\"\n")
            rc.write(f"eval \"$('{ftab}' init zsh post --rcfile zshrc)\"\n")
        shell = ["/bin/zsh", "-i", "-c", "exit"]
        cases = [
            ("/usr/bin/true", ["/usr/bin/true"], base),
            ("ftab --version (clap, no runtime)", [ftab, "--version"], base),
            ("ftab _ pre-cmd (launch only)", [ftab, "_", "pre-cmd"], base),
            ("ftab _ get-shell", [ftab, "_", "get-shell"], base),
            ("ftab _ should-figterm-launch", [ftab, "_", "should-figterm-launch"], app_up),
            ("ftab init zsh pre, app down", [ftab, "init", "zsh", "pre", "--rcfile", "zshrc"], app_down),
            ("ftab init zsh pre", [ftab, "init", "zsh", "pre", "--rcfile", "zshrc"], app_up),
            ("ftab init zsh post", [ftab, "init", "zsh", "post", "--rcfile", "zshrc"], app_up),
            ("zsh -i, no integration", shell, dict(base, ZDOTDIR=bare)),
            ("zsh -i, app down", shell, dict(app_down, ZDOTDIR=integrated)),
            ("zsh -i, app up", shell, dict(app_up, ZDOTDIR=integrated)),
        ]
        for _, argv, env in cases:
            run(argv, env)
        samples = {name: [] for name, _, _ in cases}
        for _ in range(args.rounds):
            for name, argv, env in cases:
                samples[name].append(run(argv, env))

    width = max(len(name) for name in samples)
    print(f"{'case':<{width}}  median     p90")
    for name, values in samples.items():
        values.sort()
        p90 = values[math.ceil(0.9 * len(values)) - 1]
        print(f"{name:<{width}}  {statistics.median(values):6.1f}  {p90:6.1f}")
    added = statistics.median(samples["zsh -i, app up"]) - statistics.median(samples["zsh -i, no integration"])
    print(f"integration adds {added:.1f} ms to a zsh start with the app up")


if __name__ == "__main__":
    main()
