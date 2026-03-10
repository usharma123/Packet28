#!/usr/bin/env python3
import subprocess
import sys
import threading


ROOT = "/Users/utsavsharma/Documents/GitHub/Coverage"
PACKET28 = "/usr/local/lib/node_modules/packet28/node_modules/@packet28/darwin-arm64/bin/Packet28"
IN_LOG = "/tmp/p28_mcp_in.log"
OUT_LOG = "/tmp/p28_mcp_out.log"
ERR_LOG = "/tmp/p28_mcp_err.log"


def forward(src, dst, log_path):
    with open(log_path, "ab", buffering=0) as log:
        while True:
            chunk = src.read(1)
            if not chunk:
                try:
                    dst.close()
                except Exception:
                    pass
                return
            log.write(chunk)
            dst.write(chunk)
            dst.flush()


def forward_err(src, log_path):
    with open(log_path, "ab", buffering=0) as log:
        while True:
            chunk = src.read(1)
            if not chunk:
                return
            log.write(chunk)
            sys.stderr.buffer.write(chunk)
            sys.stderr.buffer.flush()


def main() -> int:
    child = subprocess.Popen(
        [PACKET28, "mcp", "serve", "--root", ROOT],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )

    threads = [
        threading.Thread(
            target=forward,
            args=(sys.stdin.buffer, child.stdin, IN_LOG),
            daemon=True,
        ),
        threading.Thread(
            target=forward,
            args=(child.stdout, sys.stdout.buffer, OUT_LOG),
            daemon=True,
        ),
        threading.Thread(
            target=forward_err,
            args=(child.stderr, ERR_LOG),
            daemon=True,
        ),
    ]
    for thread in threads:
        thread.start()
    return child.wait()


if __name__ == "__main__":
    raise SystemExit(main())
