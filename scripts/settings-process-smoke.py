#!/usr/bin/env python3
"""Drive the Settings process the way the desktop app does, with no desktop app.

    scripts/settings-process-smoke.py target/dist/fastab

Unit tests cover the protocol from both ends. This covers what they cannot: that
the real binary reaches its socket, opens a real window, answers, and leaves when
it should, both when told to close and when the desktop app disappears. A build
that passes everything else could still fail any of that at launch. Needs a GUI
session and a Metal device; a runner without one fails at the first `Show`.

The messages are pinned in `settings_process.rs`. Exits non-zero on any failure.
"""

import json
import os
import select
import socket
import subprocess
import sys
import tempfile
import time

ANSWER_TIMEOUT = 30
EXIT_TIMEOUT = 8  # The process's own forced exit is 2 s after EOF.


def footprint_kib(pid):
    out = subprocess.run(["footprint", str(pid)], capture_output=True, text=True).stdout
    for line in out.splitlines():
        if line.strip().startswith("Footprint:"):
            value, unit = line.split()[1:3]
            return float(value) * {"KB": 1, "MB": 1024, "GB": 1024 * 1024}.get(unit, 1)
    return None


class Helper:
    def __init__(self, binary, directory):
        path = os.path.join(directory, "s.sock")
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(path)
        listener.listen(1)
        listener.settimeout(ANSWER_TIMEOUT)
        self.process = subprocess.Popen(
            [binary, "--settings-window", path],
            env={**os.environ, "HOME": directory},
            stdin=subprocess.DEVNULL,
        )
        try:
            self.connection, _ = listener.accept()
        except socket.timeout:
            raise SystemExit(self.failure("did not connect"))
        finally:
            listener.close()
            os.unlink(path)
        self.unread = b""

    def failure(self, what):
        return f"FAIL: {what} (exit status: {self.process.poll()})"

    def send(self, message):
        self.connection.sendall(json.dumps(message, separators=(",", ":")).encode() + b"\n")

    def wait_for(self, expected):
        """Milliseconds until `expected` arrives, skipping anything else."""
        started = time.monotonic()
        deadline = started + ANSWER_TIMEOUT
        while True:
            while b"\n" in self.unread:
                line, self.unread = self.unread.split(b"\n", 1)
                if json.loads(line) == expected:
                    return (time.monotonic() - started) * 1000
            remaining = deadline - time.monotonic()
            ready, _, _ = select.select([self.connection], [], [], max(remaining, 0))
            if not ready:
                raise SystemExit(self.failure(f"no {expected} within {ANSWER_TIMEOUT} s"))
            data = self.connection.recv(4096)
            if not data:
                raise SystemExit(self.failure(f"hung up before {expected}"))
            self.unread += data

    def wait_for_exit(self):
        """Milliseconds until the process is gone."""
        started = time.monotonic()
        try:
            status = self.process.wait(EXIT_TIMEOUT)
        except subprocess.TimeoutExpired:
            self.process.kill()
            raise SystemExit(f"FAIL: still running {EXIT_TIMEOUT} s later")
        if status != 0:
            raise SystemExit(f"FAIL: exit status {status}")
        return (time.monotonic() - started) * 1000


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    binary = os.path.abspath(sys.argv[1])
    # A socket path has a low length limit, and $TMPDIR on a runner can be long.
    with tempfile.TemporaryDirectory(prefix="fs", dir="/tmp") as directory:
        helper = Helper(binary, directory)
        helper.send({"Show": {"path": "/about"}})
        opened = helper.wait_for("Shown")
        print(f"window opened, answered   {opened:7.0f} ms")
        # Fonts, the first frames and the permission check settle.
        time.sleep(3)
        kib = footprint_kib(helper.process.pid)
        if kib is not None:
            print(f"footprint with the window {kib:7.0f} KiB")
        helper.send({"Show": {"path": None}})
        helper.wait_for("Shown")
        helper.send("RefreshPermissions")
        helper.send("Close")
        print(f"close to exit             {helper.wait_for_exit():7.0f} ms")

        helper = Helper(binary, directory)
        helper.send({"Show": {"path": None}})
        helper.wait_for("Shown")
        helper.connection.close()
        print(f"desktop app gone to exit  {helper.wait_for_exit():7.0f} ms")
    print("PASS")


if __name__ == "__main__":
    main()
