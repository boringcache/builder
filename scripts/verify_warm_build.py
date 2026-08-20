#!/usr/bin/env python3
"""Verify that a repeated build reused cache and produced the same image."""

from __future__ import annotations

import argparse
import hashlib
import json
import tarfile
from pathlib import Path
from typing import Any


def load_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text())


def manifest_digest(index: dict[str, Any]) -> str:
    return index["manifests"][0]["digest"]


def blob_path(layout: Path, digest: str) -> Path:
    algorithm, value = digest.split(":", 1)
    return layout / "blobs" / algorithm / value


def load_blob_json(layout: Path, descriptor: dict[str, Any]) -> dict[str, Any]:
    return load_json(blob_path(layout, descriptor["digest"]))


def tar_entries(path: Path) -> dict[str, dict[str, Any]]:
    entries: dict[str, dict[str, Any]] = {}
    with tarfile.open(path, "r:*") as archive:
        for member in archive:
            content_digest = None
            if member.isfile():
                source = archive.extractfile(member)
                if source is not None:
                    hasher = hashlib.sha256()
                    while chunk := source.read(1024 * 1024):
                        hasher.update(chunk)
                    content_digest = hasher.hexdigest()
            entries[member.name] = {
                "type": member.type.decode("ascii", errors="replace"),
                "mode": member.mode,
                "uid": member.uid,
                "gid": member.gid,
                "size": member.size,
                "link": member.linkname,
                "sha256": content_digest,
            }
    return entries


def compare_entries(seed: Path, warm: Path) -> dict[str, Any]:
    seed_entries = tar_entries(seed)
    warm_entries = tar_entries(warm)
    seed_names = set(seed_entries)
    warm_names = set(warm_entries)
    changed = [
        {
            "path": path,
            "seed": seed_entries[path],
            "warm": warm_entries[path],
        }
        for path in sorted(seed_names & warm_names)
        if seed_entries[path] != warm_entries[path]
    ]
    return {
        "seed_entries": len(seed_entries),
        "warm_entries": len(warm_entries),
        "only_seed": sorted(seed_names - warm_names)[:20],
        "only_warm": sorted(warm_names - seed_names)[:20],
        "changed": changed[:20],
        "changed_count": len(changed),
    }


def diagnose_oci_difference(seed_index: Path, warm_index: Path) -> dict[str, Any]:
    seed_layout = seed_index.parent
    warm_layout = warm_index.parent
    seed_descriptor = load_json(seed_index)["manifests"][0]
    warm_descriptor = load_json(warm_index)["manifests"][0]
    seed_manifest = load_blob_json(seed_layout, seed_descriptor)
    warm_manifest = load_blob_json(warm_layout, warm_descriptor)
    layer_differences = []
    for index, (seed_layer, warm_layer) in enumerate(
        zip(seed_manifest["layers"], warm_manifest["layers"], strict=True)
    ):
        if seed_layer["digest"] == warm_layer["digest"]:
            continue
        layer_differences.append(
            {
                "index": index,
                "seed": seed_layer["digest"],
                "warm": warm_layer["digest"],
                "entries": compare_entries(
                    blob_path(seed_layout, seed_layer["digest"]),
                    blob_path(warm_layout, warm_layer["digest"]),
                ),
            }
        )
    return {
        "seed_manifest": seed_descriptor["digest"],
        "warm_manifest": warm_descriptor["digest"],
        "seed_config": seed_manifest["config"]["digest"],
        "warm_config": warm_manifest["config"]["digest"],
        "seed_layer_count": len(seed_manifest["layers"]),
        "warm_layer_count": len(warm_manifest["layers"]),
        "layer_differences": layer_differences,
    }


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
        diagnostics = diagnose_oci_difference(args.seed_index, args.warm_index)
        print("OCI difference:\n" + json.dumps(diagnostics, indent=2))
        raise ValueError(f"image digest changed: {seed_digest} != {warm_digest}")

    print(
        f"warm cache proof: {warm_hits} hit(s), "
        f"{seed['total_ms']} ms -> {warm['total_ms']} ms, digest {warm_digest}"
    )


if __name__ == "__main__":
    main()
