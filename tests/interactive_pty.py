#!/usr/bin/env python3
"""Drive every Zed [y/N] checkpoint through a real pseudo-terminal."""

import os
import select
import signal
import sys
import time


def main() -> int:
    if len(sys.argv) < 4 or "--" not in sys.argv:
        raise SystemExit(
            "usage: interactive_pty.py <yes|no|kill-after=N> -- <command> [args...]"
        )
    mode = sys.argv[1]
    if mode not in {"yes", "no"} and not mode.startswith("kill-after="):
        raise SystemExit(f"unsupported mode: {mode}")
    kill_after = int(mode.split("=", 1)[1]) if mode.startswith("kill-after=") else None
    split = sys.argv.index("--")
    command = sys.argv[split + 1 :]
    if not command:
        raise SystemExit("missing command")

    pid, fd = os.forkpty()
    if pid == 0:
        environment = os.environ.copy()
        # This harness intentionally simulates a human terminal even when the
        # parent test runner is CI. Production commands still detect CI unless
        # a caller explicitly supplies the same documented override.
        environment["ZED_PKG_FORCE_CI"] = "0"
        os.execvpe(command[0], command, environment)

    deadline = time.monotonic() + 120
    output = bytearray()
    scanned = 0
    prompts = 0
    status = None
    killed = False
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                try:
                    chunk = os.read(fd, 65536)
                except OSError:
                    chunk = b""
                if chunk:
                    output.extend(chunk)
                    sys.stdout.buffer.write(chunk)
                    sys.stdout.buffer.flush()
                    while True:
                        found = output.find(b"[y/N]", scanned)
                        if found < 0:
                            scanned = max(scanned, len(output) - 4)
                            break
                        scanned = found + len(b"[y/N]")
                        prompts += 1
                        if kill_after is not None and prompts == kill_after:
                            os.kill(pid, signal.SIGKILL)
                            killed = True
                            break
                        os.write(
                            fd,
                            b"yes\n" if mode == "yes" or kill_after is not None else b"no\n",
                        )
                        if mode == "no":
                            break
            waited, raw = os.waitpid(pid, os.WNOHANG)
            if waited == pid:
                status = raw
                break
        if status is None:
            os.kill(pid, signal.SIGKILL)
            _, status = os.waitpid(pid, 0)
            raise SystemExit("pseudo-terminal command timed out")
    finally:
        try:
            os.close(fd)
        except OSError:
            pass

    if prompts == 0:
        raise SystemExit("command exited before showing an interactive checkpoint")
    exit_code = os.waitstatus_to_exitcode(status)
    if kill_after is not None:
        if not killed or prompts != kill_after or exit_code == 0:
            raise SystemExit(
                f"expected hard exit at prompt {kill_after}; prompts={prompts} exit={exit_code}"
            )
        return 0
    if mode == "yes":
        return exit_code
    if exit_code == 0:
        raise SystemExit("negative confirmation unexpectedly succeeded")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
