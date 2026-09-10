#!/usr/bin/env python3
"""Run deterministic Rust fixtures in isolated processes; report, never time-gate."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
CASES = ("feed_parse", "feed_ingest", "ingest_sources", "report_render", "extract_fixture", "ai_orchestration")


def sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--executable", type=Path, help="reuse an already built perf test binary")
    parser.add_argument("--binary", type=Path, help="associated release CLI binary, for size and digest")
    parser.add_argument("--time-command", type=Path, help="GNU time executable (default: PATH)")
    args = parser.parse_args()
    if not 1 <= args.repeat <= 20:
        parser.error("--repeat must be in 1..=20")
    time_command = args.time_command or shutil.which("time")
    if platform.system() != "Linux" or not time_command:
        parser.error("Linux GNU time is required for peak RSS; install the time package or pass --time-command")
    executable = args.executable
    if executable is None:
        built = subprocess.run(
            ["cargo", "test", "--release", "--locked", "-p", "rss-ai-news-runtime", "--test", "perf", "--no-run", "--message-format=json"],
            cwd=ROOT, check=True, text=True, stdout=subprocess.PIPE,
        )
        for line in built.stdout.splitlines():
            item = json.loads(line)
            if item.get("reason") == "compiler-artifact" and item.get("target", {}).get("name") == "perf" and item.get("executable"):
                executable = Path(item["executable"])
        if executable is None:
            raise RuntimeError("Cargo did not report the perf executable")
    executable = executable.resolve()
    samples = []
    for case in CASES:
        for sample in range(args.repeat):
            with tempfile.TemporaryDirectory(prefix="rss-perf-") as temp:
                rss_file = Path(temp) / "rss.txt"
                result = subprocess.run(
                    [str(time_command), "-f", "%M", "-o", str(rss_file), str(executable), "--ignored", "--exact", case, "--nocapture", "--test-threads=1"],
                    cwd=ROOT, check=True, text=True, stdout=subprocess.PIPE,
                )
                stdout = result.stdout
                peak = int(rss_file.read_text().strip())
            found = False
            for line in stdout.splitlines():
                if "PERF " in line:
                    measurement = json.loads(line.split("PERF ", 1)[1])
                    measurement.update(sample=sample, peak_rss_kib=peak)
                    samples.append(measurement)
                    found = True
            if not found:
                raise RuntimeError(f"{case} produced no measurement")
    medians = {}
    for case in sorted({row["case"] for row in samples}):
        rows = [row for row in samples if row["case"] == case]
        medians[case] = {
            "items": rows[0]["items"],
            "elapsed_ns": statistics.median(row["elapsed_ns"] for row in rows),
            "peak_rss_kib": statistics.median(row["peak_rss_kib"] for row in rows),
        }
    binary = args.binary or Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")) / "release/rss-ai-news"
    report = {
        "schema_version": 1,
        "executable_sha256": sha256(executable),
        "release_binary_sha256": sha256(binary) if binary.exists() else None,
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True)),
        "platform": platform.platform(),
        "rustc": subprocess.check_output(["rustc", "--version"], cwd=ROOT, text=True).strip(),
        "release_binary_bytes": binary.stat().st_size if binary.exists() else None,
        "rss_scope": "isolated test process including fixture setup; feed_ingest sizes share one process",
        "medians": medians,
        "samples": samples,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(medians, indent=2))
    print(f"Report: {args.output}")


if __name__ == "__main__":
    main()
