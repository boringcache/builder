#!/usr/bin/env python3
"""Verify that a repeated build reused cache and produced the same image."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def load_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text())


def manifest_digest(index: dict[str, Any]) -> str:
    return index["manifests"][0]["digest"]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--seed-timings", type=Path, required=True)
    parser.add_argument("--warm-timings", type=Path, required=True)
    parser.add_argument("--seed-index", type=Path, required=True)
    parser.add_argument("--warm-index", type=Path, required=True)
    args = parser.parse_args()

    seed = load_json(args.seed_timings)
    warm = load_json(args.warm_timings)
    seed_digest = manifest_digest(load_json(args.seed_index))
    warm_digest = manifest_digest(load_json(args.warm_index))
    warm_hits = warm["cache"]["hit_count"]

    if warm_hits < 1:
        raise ValueError("the repeated build did not reuse any cached operations")
    if seed_digest != warm_digest:
        raise ValueError(f"image digest changed: {seed_digest} != {warm_digest}")

    print(
        f"warm cache proof: {warm_hits} hit(s), "
        f"{seed['total_ms']} ms -> {warm['total_ms']} ms, digest {warm_digest}"
    )


if __name__ == "__main__":
    main()
