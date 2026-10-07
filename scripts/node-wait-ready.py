#!/usr/bin/env python3
"""Wait for a local mainnet validator without printing RPC credentials."""
import base64
import json
from pathlib import Path
import sys
import time
import urllib.error
import urllib.request

def check_ready(root, minimum):
    """Require authenticated mainnet RPC and Zakura's own readiness checks."""
    try:
        cookie = (root / "auth/.cookie").read_bytes().strip()
        request = urllib.request.Request("http://127.0.0.1:8232", data=json.dumps({
            "jsonrpc": "2.0", "id": 1, "method": "getblockchaininfo", "params": [],
        }).encode(), headers={"Content-Type": "application/json",
                              "Authorization": "Basic " + base64.b64encode(cookie).decode()})
        with urllib.request.urlopen(request, timeout=10) as response:
            reply = json.load(response)
        if reply.get("error"):
            raise RuntimeError("validator RPC is not ready")
        info = reply["result"]
        if info.get("chain") != "main":
            raise ValueError("refusing a validator that is not mainnet")
        height = info.get("blocks", 0)
        behind = max(0, info.get("estimatedheight", info.get("headers", height)) - height)
        # Zakura does not expose zcashd's initialblockdownload field. Its own
        # readiness endpoint checks peers, recent block time and sync lag.
        if height >= minimum and behind <= 2:
            with urllib.request.urlopen("http://127.0.0.1:8234/ready", timeout=10) as response:
                ready = response.status == 200
            if ready:
                return True, {"phase": "validator-ready", "height": height}
        status = {"phase": "validator-catching-up", "height": height, "behind": behind,
                  "verificationprogress": info.get("verificationprogress")}
    except ValueError:
        raise
    except urllib.error.HTTPError as error:
        error.close()
        status = {"phase": "waiting-for-validator"}
    except (OSError, RuntimeError, KeyError):
        status = {"phase": "waiting-for-validator"}
    return False, status


def wait(root):
    minimum = json.loads((root / "metadata/snapshot.json").read_text())["height"]
    last_report = float("-inf")
    while True:
        ready, status = check_ready(root, minimum)
        if ready:
            print(f"Mainnet validator ready at {status['height']}; starting Zaino", flush=True)
            return
        now = time.monotonic()
        if now - last_report >= 60:
            print(json.dumps(status), flush=True)
            last_report = now
        time.sleep(10)


if __name__ == "__main__":
    wait(Path(sys.argv[1]))
