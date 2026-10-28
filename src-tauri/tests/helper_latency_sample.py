import json
import math
from pathlib import Path
import statistics
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "resources" / "desktop_control.py"
SAMPLES = 20


def percentile(values, percentile):
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, math.ceil(percentile * len(ordered)) - 1))
    return ordered[index]


def cold_samples():
    timings = []
    for _ in range(SAMPLES):
        started = time.perf_counter()
        completed = subprocess.run(
            [sys.executable, str(HELPER)],
            input=json.dumps({"operation": "ping"}),
            text=True,
            capture_output=True,
            check=True,
        )
        response = json.loads(completed.stdout)
        if not response.get("ok") or not response.get("result", {}).get("ready"):
            raise RuntimeError(f"cold helper ping failed: {response}")
        timings.append((time.perf_counter() - started) * 1000.0)
    return timings


def persistent_samples():
    process = subprocess.Popen(
        [sys.executable, str(HELPER), "--serve"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1,
    )
    assert process.stdin is not None
    assert process.stdout is not None
    timings = []
    try:
        for request_id in range(1, SAMPLES + 1):
            started = time.perf_counter()
            process.stdin.write(
                json.dumps(
                    {
                        "id": request_id,
                        "request": {"operation": "ping"},
                    }
                )
                + "\n"
            )
            process.stdin.flush()
            line = process.stdout.readline()
            if not line:
                raise RuntimeError("persistent helper exited before replying")
            response = json.loads(line)
            if (
                response.get("id") != request_id
                or not response.get("ok")
                or not response.get("result", {}).get("ready")
            ):
                raise RuntimeError(f"persistent helper ping failed: {response}")
            timings.append((time.perf_counter() - started) * 1000.0)
    finally:
        process.terminate()
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=2)
    return timings


def summary(label, timings):
    return {
        "mode": label,
        "samples": len(timings),
        "p50Ms": round(statistics.median(timings), 3),
        "p95Ms": round(percentile(timings, 0.95), 3),
        "minMs": round(min(timings), 3),
        "maxMs": round(max(timings), 3),
    }


def main():
    cold = cold_samples()
    warm = persistent_samples()
    report = {
        "note": "Informational only; no machine-specific timing threshold is enforced.",
        "coldOneShot": summary("cold-one-shot", cold),
        "persistent": summary("persistent", warm),
    }
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
