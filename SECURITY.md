# Security

## Supported versions

Security fixes are applied to the latest published release. During the alpha,
users should upgrade to the newest prerelease before reporting a problem.

## Report a vulnerability

Please do not open a public issue for a suspected vulnerability.

Email [security@boringcache.com](mailto:security@boringcache.com) or, once the
repository is public, use
[GitHub private vulnerability reporting](https://github.com/boringcache/builder/security/advisories/new).
Include the affected version, impact, reproduction steps, and any suggested
mitigation. We will acknowledge a complete report as soon as practical and
coordinate disclosure after a fix is available.

## Build trust

Public release binaries include SHA-256 checksums and GitHub build-provenance
attestations. Verify an attestation with:

```sh
gh attestation verify boringbuilder-linux-amd64 --repo boringcache/builder
```
