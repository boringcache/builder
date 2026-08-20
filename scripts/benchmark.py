#!/usr/bin/env python3
"""Measure one cold and one warm Dockerfile build on an ephemeral Linux runner."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import time
from datetime import UTC, datetime
from pathlib import Path
from typing import Any


def peak_rss_kib(path: Path) -> int:
    prefix = "Maximum resident set size (kbytes):"
    for line in path.read_text().splitlines():
        if line.strip().startswith(prefix):
            return int(line.split(":", 1)[1].strip())
    raise ValueError(f"peak RSS was not recorded in {path}")


def layout_digest(path: Path) -> str:
    index = json.loads((path / "index.json").read_text())
    return index["manifests"][0]["digest"]


def directory_size(path: Path) -> int:
    return sum(item.stat().st_size for item in path.rglob("*") if item.is_file())


def run_build(
    label: str,
    binary: Path,
    dockerfile: Path,
    platform: str,
    cache_dir: Path,
    output_dir: Path,
) -> dict[str, Any]:
    image = output_dir / f"{label}.oci"
    timings_path = output_dir / f"{label}-timings.json"
    resource_path = output_dir / f"{label}-resources.txt"
    command = [
        "/usr/bin/time",
        "-v",
        "-o",
        str(resource_path),
        str(binary),
        "build",
        "-f",
        str(dockerfile),
        "--platform",
        platform,
        "--format",
        "oci",
        "-o",
        str(image),
        "--cache-dir",
        str(cache_dir),
        "--timings-json",
        str(timings_path),
    ]

    started = time.perf_counter()
    subprocess.run(command, check=True)
    wall_ms = round((time.perf_counter() - started) * 1000)
    timings = json.loads(timings_path.read_text())
    result = {
        "wall_ms": wall_ms,
        "peak_rss_kib": peak_rss_kib(resource_path),
        "image_bytes": directory_size(image),
        "manifest_digest": layout_digest(image),
        "builder_timings": timings,
    }
    shutil.rmtree(image)
    return result


def markdown(report: dict[str, Any]) -> str:
    cold = report["runs"]["cold"]
    warm = report["runs"]["warm"]
    speedup = cold["wall_ms"] / warm["wall_ms"] if warm["wall_ms"] else 0
    return "\n".join(
        [
            "# Boringbuilder benchmark",
            "",
            f"Commit: `{report['commit']}`  ",
            f"Platform: `{report['platform']}`  ",
            f"Cold/warm digest: `{cold['manifest_digest']}`",
            "",
            "| Run | Wall time | Peak RSS | OCI bytes | Cached operations |",
            "| --- | ---: | ---: | ---: | ---: |",
            (
                f"| Cold | {cold['wall_ms'] / 1000:.2f}s | "
                f"{cold['peak_rss_kib'] / 1024:.1f} MiB | "
                f"{cold['image_bytes']:,} | "
                f"{cold['builder_timings']['cache']['hit_count']} |"
            ),
            (
                f"| Warm | {warm['wall_ms'] / 1000:.2f}s | "
                f"{warm['peak_rss_kib'] / 1024:.1f} MiB | "
                f"{warm['image_bytes']:,} | "
                f"{warm['builder_timings']['cache']['hit_count']} |"
            ),
            "",
            f"Warm speedup: **{speedup:.2f}×**",
            "",
            "Hosted-runner results are trend data, not a stable hardware benchmark.",
            "",
        ]
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--binary", type=Path, default=Path("target/release/boringbuilder")
    )
    parser.add_argument("--dockerfile", type=Path, default=Path("Dockerfile"))
    parser.add_argument("--platform", default="linux/amd64")
    parser.add_argument("--output", type=Path, default=Path("dist/benchmark"))
    args = parser.parse_args()

    binary = args.binary.resolve()
    dockerfile = args.dockerfile.resolve()
    output = args.output.resolve()
    cache_dir = output / "cache"
    shutil.rmtree(output, ignore_errors=True)
    output.mkdir(parents=True)

    report = {
        "schema_version": 1,
        "recorded_at": datetime.now(UTC).isoformat(),
        "commit": os.environ.get("GITHUB_SHA", "local"),
        "platform": args.platform,
        "runs": {
            "cold": run_build(
                "cold", binary, dockerfile, args.platform, cache_dir, output
            ),
            "warm": run_build(
                "warm", binary, dockerfile, args.platform, cache_dir, output
            ),
        },
    }
    cold_digest = report["runs"]["cold"]["manifest_digest"]
    warm_digest = report["runs"]["warm"]["manifest_digest"]
    if cold_digest != warm_digest:
        raise ValueError(
            f"cold and warm OCI digests differ: {cold_digest} != {warm_digest}"
        )

    (output / "benchmark.json").write_text(json.dumps(report, indent=2) + "\n")
    summary = markdown(report)
    (output / "benchmark.md").write_text(summary)
    print(summary)


if __name__ == "__main__":
    main()
