# TODO

- Extend the scheduled OCI benchmark with a representative tar.zst export.
- Verify deterministic artifact and OCI digests across clean machines.
- Publish and install `v0.1.0-alpha.1` end to end.
- Before making the repository public, enable secret scanning, push protection,
  private vulnerability reporting, immutable releases, first-time contributor
  workflow approval, and Harden-Runner for CI, benchmarks, and releases; then
  run CodeQL and Scorecard once. Harden-Runner stays in public-only jobs until
  then because its pre-hook still runs and reports a subscription warning when
  a step-level condition skips it in a private repository.
- Promote a proven alpha to the first stable `v0.1.0` release.
- Archive the previous repository after this product is established.
