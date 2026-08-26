# Security

## Supported versions

Security fixes are applied to the latest published release. During the alpha, upgrade to the newest prerelease before
reporting a problem.

## Report a vulnerability

Do not open a public issue for a suspected vulnerability. Email
[security@boringcache.com](mailto:security@boringcache.com) or use
[GitHub private vulnerability reporting](https://github.com/boringcache/builder/security/advisories/new).

Include the affected version, impact, reproduction steps, and any suggested mitigation. Do not include credentials,
private source code, or registry tokens unless the maintainers provide a secure channel.

## Build boundaries

BoringBuilder sends the selected project context to a local Dagger Engine. `.gitignore` and the built-in source
exclusions reduce accidental context, but users remain responsible for excluding secrets from the project and image.
Registry authentication is handled by the selected runtime and Dagger; BoringBuilder does not accept credentials on
the command line.

`config/boringbuilder.rb` and files passed with `--config` are executable Ruby loaded on the host. Review recipes
before building source you do not trust.
