#!/usr/bin/env python3
"""Run a command under a real PTY and answer Zed's confirmation prompt."""

import os
import select
import signal
import sys
import time


def main() -> int:
    if len(sys.argv) < 4 or "--" not in sys.argv:
        raise SystemExit("usage: manifestless_pty.py <yes|no|eof> -- <command> [args...]")
    mode = sys.argv[1]
    if mode not in {"yes", "no", "eof"}:
        raise SystemExit(f"unsupported mode: {mode}")
    split = sys.argv.index("--")
    command = sys.argv[split + 1 :]
    if not command:
        raise SystemExit("missing command")

    pid, fd = os.forkpty()
    if pid == 0:
        os.execvp(command[0], command)

    deadline = time.monotonic() + 60
    output = bytearray()
    answered = False
    status = None
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
                    if not answered and b"[y/N]" in output:
                        os.write(fd, {"yes": b"yes\n", "no": b"no\n", "eof": b"\x04"}[mode])
                        answered = True
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

    if not answered:
        raise SystemExit("command exited before showing the manifestless consent prompt")
    return os.waitstatus_to_exitcode(status)


if __name__ == "__main__":
    raise SystemExit(main())
