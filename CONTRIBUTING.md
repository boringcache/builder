# Contributing

Keep changes small, focused, and easy for the next reader to understand.

Read [SOUL.md](SOUL.md) and [STYLE.md](STYLE.md) first. Coding agents should also follow [AGENTS.md](AGENTS.md).

1. Open an issue before a large product-scope change.
2. Create a branch from `main`.
3. Run `bin/ci` and the relevant live Dagger smoke test.
4. Open a pull request with the user-visible outcome and verification evidence.

The gemspec and CI define supported Ruby versions; the checked-in toolchain is the development default. Keep Dagger
GraphQL additions in Dagger Ruby rather than issuing ad hoc queries from BoringBuilder. New Rails conventions should
fail with an actionable message when the gem cannot build safely.

Commits must be signed. Pull-request tests must not require repository secrets. Report vulnerabilities through
[SECURITY.md](SECURITY.md), not a public issue.
