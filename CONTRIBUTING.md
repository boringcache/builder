# Contributing

Keep changes small, focused, and easy to review.

1. Open an issue before a large feature or product-scope change.
2. Create a branch from `main`.
3. Run `make verify` and the relevant build example.
4. Open a pull request with the problem, approach, and verification evidence.

Commits must be signed. CI runs with read-only permissions and no repository
secrets on pull requests, so tests must not depend on privileged credentials.

Report vulnerabilities through [SECURITY.md](SECURITY.md), not a public issue.
