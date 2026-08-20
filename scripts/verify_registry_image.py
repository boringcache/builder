#!/usr/bin/env python3
"""Pull and verify an OCI image directly through the Registry HTTP API."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any
from urllib.request import Request, urlopen

MANIFEST_ACCEPT = (
    "application/vnd.oci.image.index.v1+json, "
    "application/vnd.oci.image.manifest.v1+json, "
    "application/vnd.docker.distribution.manifest.list.v2+json, "
    "application/vnd.docker.distribution.manifest.v2+json"
)


def fetch(url: str, accept: str | None = None) -> bytes:
    headers = {"Accept": accept} if accept else {}
    with urlopen(Request(url, headers=headers), timeout=60) as response:
        return response.read()


def verify_blob(payload: bytes, descriptor: dict[str, Any]) -> None:
    digest = descriptor["digest"]
    algorithm, expected = digest.split(":", 1)
    if algorithm != "sha256":
        raise ValueError(f"unsupported digest algorithm: {algorithm}")
    actual = hashlib.sha256(payload).hexdigest()
    if actual != expected:
        raise ValueError(f"digest mismatch for {digest}")
    if len(payload) != descriptor["size"]:
        raise ValueError(f"size mismatch for {digest}")


def fetch_descriptor(
    registry: str, repository: str, descriptor: dict[str, Any]
) -> bytes:
    payload = fetch(f"{registry}/v2/{repository}/blobs/{descriptor['digest']}")
    verify_blob(payload, descriptor)
    return payload


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--registry", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--reference", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    registry = args.registry.rstrip("/")
    fetch(f"{registry}/v2/")

    tagged_payload = fetch(
        f"{registry}/v2/{args.repository}/manifests/{args.reference}",
        MANIFEST_ACCEPT,
    )
    tagged = json.loads(tagged_payload)

    if "manifests" in tagged:
        candidates = [
            descriptor
            for descriptor in tagged["manifests"]
            if descriptor.get("platform", {}).get("os") == "linux"
            and descriptor.get("platform", {}).get("architecture") == "amd64"
        ]
        descriptor = candidates[0] if candidates else tagged["manifests"][0]
        manifest_payload = fetch(
            f"{registry}/v2/{args.repository}/manifests/{descriptor['digest']}",
            MANIFEST_ACCEPT,
        )
        verify_blob(manifest_payload, descriptor)
        manifest_digest = descriptor["digest"]
        manifest = json.loads(manifest_payload)
        manifest_extra_bytes = len(manifest_payload)
    else:
        manifest_payload = tagged_payload
        manifest_digest = f"sha256:{hashlib.sha256(tagged_payload).hexdigest()}"
        manifest = tagged
        manifest_extra_bytes = 0

    if manifest.get("schemaVersion") != 2:
        raise ValueError("registry returned an unsupported manifest")
    if not manifest.get("layers"):
        raise ValueError("registry manifest contains no image layers")

    config_payload = fetch_descriptor(registry, args.repository, manifest["config"])
    config = json.loads(config_payload)
    entrypoint = config.get("config", {}).get("Entrypoint")
    if entrypoint != ["/usr/local/bin/boringbuilder"]:
        raise ValueError(f"unexpected image entrypoint: {entrypoint!r}")

    pulled_bytes = len(tagged_payload) + manifest_extra_bytes + len(config_payload)
    for layer in manifest["layers"]:
        pulled_bytes += len(fetch_descriptor(registry, args.repository, layer))

    report = {
        "reference": f"{args.registry.rstrip('/')}/{args.repository}:{args.reference}",
        "manifest_digest": manifest_digest,
        "layers": len(manifest["layers"]),
        "pulled_bytes": pulled_bytes,
        "entrypoint": entrypoint,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(
        f"verified {report['reference']} "
        f"({report['layers']} layers, {report['pulled_bytes']} bytes)"
    )


if __name__ == "__main__":
    main()
