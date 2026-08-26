# BoringBuilder style

Code should feel like a clear explanation of the build it performs. These are defaults with reasons, not laws. When
the code, security boundary, or user experience calls for something different, make that choice visible.

## Ruby

- Write ordinary, readable Ruby. Prefer a small object or method over a framework made for a possibility.
- Arrange a class so its public story reads from top to bottom: class methods, public initialization and entry points,
  supporting public methods, then private implementation.
- Keep methods close to invocation order when that makes the flow easier to follow.
- Prefer expanded conditionals when both branches matter. Use an early return near the start of a method when it
  removes an exceptional or invalid case.
- Reserve a bang method for a meaningful counterpart without a bang.
- Follow RuboCop for mechanical style. The configuration is the executable baseline shared by editors and CI.

## Product language

- Describe BoringBuilder as an application builder. Rails is the first-class convention, not the boundary.
- Lead with the command and the outcome. Explain Dagger, Mise, caching, or exporters only when they help someone make
  the next decision.
- Write errors and documentation for a tired human. Be direct, specific, and calm.

## Changes

- Find the closest existing shape before introducing a new one.
- Keep provider details behind cache and exporter adapters. Keep build recipes ordinary Dagger Ruby over the shared
  Mise foundation.
- Surface invariants from CI, security policy, the gemspec, and public APIs; do not restate version promises in prose
  where they can quietly go stale.
- Test the behavior at its narrowest boundary, then run `bin/ci`. Changes to the Dagger graph also need a live build.
- Before calling a change done, attack the diff: look for accidental complexity, secret exposure, private paths,
  generated files, misleading examples, and behavior that was only mocked.
