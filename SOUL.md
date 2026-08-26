# The soul of BoringBuilder

Building an application should feel calm.

BoringBuilder exists for people who like the way Ruby and Rails make difficult work feel ordinary. They want a
production build without learning another configuration language, maintaining a pile of shell scripts, or giving up
control to a black box.

The goal is the ease of a buildpack with the clarity of a program. Dagger provides the build graph. Ruby makes that
graph readable. BoringBuilder adds strong conventions for the work most applications have in common.

## What we believe

- `boringbuilder build` should be enough for a conventional Rails application.
- Rails is the first-class path, not the limit of the product.
- Any application can own a small Ruby recipe when its build needs are different.
- Mise should give every application the same predictable toolchain foundation without making developers think
  about toolchain plumbing.
- A build should produce the same useful result as a filesystem artifact, OCI image, or Docker image.
- Caching should be automatic, close to the step that uses it, and easy to understand.
- Local builds should stay local. Shared BoringCache credentials should make the same steps portable across fresh
  machines and CI runners.
- Secrets belong in secret channels, never command lines, images, archives, plans, or logs.

## Our taste

We prefer a small amount of ordinary Ruby over a large amount of YAML. We choose conventions that solve a real
production build, then leave an honest extension point for applications that are different. We would rather add one
clear method than invent a framework around a possibility.

Errors should tell a tired person what happened and what to do next. Defaults should be safe. Generated files should
be short enough to understand in one sitting. Public documentation should sound like one human helping another.

## The human reason

The human behind BoringBuilder was tired of build tooling asking people to choose between convenience and control.
They should not have to. A build can be simple, programmatic, fast, repeatable, and owned by the application at the
same time.

That is the standard: Rails-shaped happiness, buildpack-shaped ease, and Dagger-shaped control.
