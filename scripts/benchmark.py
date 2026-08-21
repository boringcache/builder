#!/usr/bin/env python3
"""Compare Dockerfile builds with boringbuilder and BoringCache-managed BuildKit."""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import time
from dataclasses import dataclass
from datetime import UTC, datetime
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class Settings:
    binary: Path
    boringcache: Path
    dockerfile: Path
    platform: str
    output: Path
    cache_workspace: str
    cache_tag: str
    cache_port: int
    cache_epoch: str


def command_output(command: list[str]) -> str:
    return subprocess.run(
        command,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    ).stdout.strip()


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


def run_timed(command: list[str], resource_path: Path, log_path: Path) -> int:
    measured_command = [
        "/usr/bin/time",
        "-v",
        "-o",
        str(resource_path),
        *command,
    ]
    print(f"$ {' '.join(measured_command)}", flush=True)
    started = time.perf_counter()
    with log_path.open("w") as log:
        process = subprocess.Popen(
            measured_command,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
        )
        assert process.stdout is not None
        for line in process.stdout:
            print(line, end="", flush=True)
            log.write(line)
        return_code = process.wait()
    if return_code:
        raise subprocess.CalledProcessError(return_code, measured_command)
    return round((time.perf_counter() - started) * 1000)


def run_boringbuilder(
    label: str,
    settings: Settings,
) -> dict[str, Any]:
    image = settings.output / f"boringbuilder-{label}.oci"
    timings_path = settings.output / f"boringbuilder-{label}-timings.json"
    resource_path = settings.output / f"boringbuilder-{label}-resources.txt"
    log_path = settings.output / f"boringbuilder-{label}.log"
    wall_ms = run_timed(
        [
            str(settings.binary),
            "build",
            "-f",
            str(settings.dockerfile),
            "--platform",
            settings.platform,
            "--build-arg",
            f"BORINGBUILDER_CACHE_EPOCH={settings.cache_epoch}",
            "--format",
            "oci",
            "-o",
            str(image),
            "--cache-dir",
            str(settings.output / "boringbuilder-cache"),
            "--cache",
            "boringcache",
            "--cache-workspace",
            settings.cache_workspace,
            "--cache-bin",
            str(settings.boringcache),
            "--timings-json",
            str(timings_path),
        ],
        resource_path,
        log_path,
    )
    timings = json.loads(timings_path.read_text())
    result = {
        "wall_ms": wall_ms,
        "command_peak_rss_kib": peak_rss_kib(resource_path),
        "image_bytes": directory_size(image),
        "manifest_digest": layout_digest(image),
        "cached_operations": timings["cache"]["hit_count"],
        "builder_timings": timings,
    }
    shutil.rmtree(image)
    return result


def run_buildkit(
    label: str,
    settings: Settings,
) -> dict[str, Any]:
    image = settings.output / f"docker-buildkit-{label}.oci"
    resource_path = settings.output / f"docker-buildkit-{label}-resources.txt"
    log_path = settings.output / f"docker-buildkit-{label}.log"
    wall_ms = run_timed(
        [
            str(settings.boringcache),
            "docker",
            "--workspace",
            settings.cache_workspace,
            "--tag",
            settings.cache_tag,
            "--tool-cache",
            f"sccache:{settings.cache_tag}-sccache",
            "--mount-cache",
            "--port",
            str(settings.cache_port),
            "--fail-on-cache-error",
            "--",
            "docker",
            "buildx",
            "build",
            "--file",
            str(settings.dockerfile),
            "--platform",
            settings.platform,
            "--build-arg",
            f"BORINGBUILDER_CACHE_EPOCH={settings.cache_epoch}",
            "--provenance=false",
            "--sbom=false",
            "--progress=plain",
            "--output",
            f"type=oci,dest={image},tar=false",
            str(settings.dockerfile.parent),
        ],
        resource_path,
        log_path,
    )
    cached_operations = len(
        re.findall(r"^#[0-9]+ CACHED$", log_path.read_text(), flags=re.MULTILINE)
    )
    result = {
        "wall_ms": wall_ms,
        "command_peak_rss_kib": peak_rss_kib(resource_path),
        "image_bytes": directory_size(image),
        "manifest_digest": layout_digest(image),
        "cached_operations": cached_operations,
    }
    shutil.rmtree(image)
    return result


def assert_reproducible(builder: str, runs: dict[str, dict[str, Any]]) -> None:
    cold_digest = runs["cold"]["manifest_digest"]
    warm_digest = runs["warm"]["manifest_digest"]
    if cold_digest != warm_digest:
        raise ValueError(
            f"{builder} warm OCI digest differs from cold: "
            f"{warm_digest} != {cold_digest}"
        )


def format_run(
    builder: str, label: str, run: dict[str, Any], speedup: float | None = None
) -> str:
    speedup_text = f"{speedup:.2f}x" if speedup is not None else "-"
    return (
        f"| {builder} | {label} | {run['wall_ms'] / 1000:.2f}s | "
        f"{run['cached_operations']} | {run['image_bytes']:,} | {speedup_text} |"
    )


def markdown(report: dict[str, Any]) -> str:
    boringbuilder = report["builders"]["boringbuilder"]["runs"]
    buildkit = report["builders"]["docker_buildkit_boringcache"]["runs"]
    boringbuilder_speedup = (
        boringbuilder["cold"]["wall_ms"] / boringbuilder["warm"]["wall_ms"]
    )
    buildkit_speedup = buildkit["cold"]["wall_ms"] / buildkit["warm"]["wall_ms"]
    cold_ratio = boringbuilder["cold"]["wall_ms"] / buildkit["cold"]["wall_ms"]
    warm_ratio = boringbuilder["warm"]["wall_ms"] / buildkit["warm"]["wall_ms"]
    return "\n".join(
        [
            "# Dockerfile builder comparison",
            "",
            f"Commit: `{report['commit']}`  ",
            f"Platform: `{report['platform']}`  ",
            f"Execution order: `{' -> '.join(report['execution_order'])}`",
            "",
            "| Builder | Run | Wall time | Cached operations | OCI bytes | Warm speedup |",
            "| --- | --- | ---: | ---: | ---: | ---: |",
            format_run("boringbuilder", "Cold", boringbuilder["cold"]),
            format_run(
                "boringbuilder",
                "Warm",
                boringbuilder["warm"],
                boringbuilder_speedup,
            ),
            format_run("Docker BuildKit + BoringCache", "Cold", buildkit["cold"]),
            format_run(
                "Docker BuildKit + BoringCache",
                "Warm",
                buildkit["warm"],
                buildkit_speedup,
            ),
            "",
            (
                f"Boringbuilder/Docker wall-time ratio: cold **{cold_ratio:.2f}x**, "
                f"warm **{warm_ratio:.2f}x**."
            ),
            "",
            (
                "Each builder must reproduce its own cold OCI digest on the warm build. "
                "Cross-builder digests and cache-operation counts are not directly comparable."
            ),
            "",
            (
                "Reported command RSS is retained in the JSON evidence but omitted here: "
                "Docker's CLI measurement excludes BuildKit and BoringCache daemons and "
                "would be misleading."
            ),
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
    parser.add_argument("--boringcache", type=Path, default=Path("boringcache"))
    parser.add_argument("--dockerfile", type=Path, default=Path("Dockerfile"))
    parser.add_argument("--platform", default="linux/amd64")
    parser.add_argument("--output", type=Path, default=Path("dist/benchmark"))
    parser.add_argument("--cache-workspace", default="boringcache/boringbuilder")
    parser.add_argument("--cache-tag", default="builder-comparison-local")
    parser.add_argument("--cache-port", type=int, default=22244)
    parser.add_argument(
        "--cache-epoch",
        default=os.environ.get("GITHUB_RUN_ID", "local"),
    )
    args = parser.parse_args()

    binary = args.binary.resolve()
    boringcache_command = shutil.which(str(args.boringcache))
    boringcache = (
        Path(boringcache_command).resolve()
        if boringcache_command
        else args.boringcache.resolve()
    )
    dockerfile = args.dockerfile.resolve()
    output = args.output.resolve()
    shutil.rmtree(output, ignore_errors=True)
    output.mkdir(parents=True)
    settings = Settings(
        binary=binary,
        boringcache=boringcache,
        dockerfile=dockerfile,
        platform=args.platform,
        output=output,
        cache_workspace=args.cache_workspace,
        cache_tag=args.cache_tag,
        cache_port=args.cache_port,
        cache_epoch=args.cache_epoch,
    )

    report: dict[str, Any] = {
        "schema_version": 2,
        "recorded_at": datetime.now(UTC).isoformat(),
        "commit": os.environ.get("GITHUB_SHA", "local"),
        "platform": args.platform,
        "execution_order": ["boringbuilder", "docker_buildkit_boringcache"],
        "builders": {
            "boringbuilder": {
                "version": command_output([str(binary), "--version"]),
                "runs": {},
            },
            "docker_buildkit_boringcache": {
                "boringcache_version": command_output([str(boringcache), "--version"]),
                "docker_version": command_output(
                    ["docker", "version", "--format", "{{.Client.Version}}"]
                ),
                "buildx_version": command_output(["docker", "buildx", "version"]),
                "runs": {},
            },
        },
    }

    boringbuilder_runs = report["builders"]["boringbuilder"]["runs"]
    for label in ("cold", "warm"):
        boringbuilder_runs[label] = run_boringbuilder(label, settings)
    assert_reproducible("boringbuilder", boringbuilder_runs)

    buildkit_runs = report["builders"]["docker_buildkit_boringcache"]["runs"]
    for label in ("cold", "warm"):
        buildkit_runs[label] = run_buildkit(label, settings)
    assert_reproducible("Docker BuildKit + BoringCache", buildkit_runs)

    (output / "benchmark.json").write_text(json.dumps(report, indent=2) + "\n")
    summary = markdown(report)
    (output / "benchmark.md").write_text(summary)
    print(summary)


if __name__ == "__main__":
    main()
