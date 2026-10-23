# Releases

Release-please cuts releases from Conventional Commits on `main`; a workflow publishes the crates and binaries when the GitHub release is published. Nobody runs `cargo publish` by hand.

## Flow

1. PR titles follow Conventional Commits (enforced by the `Pull request title` workflow) and become the squash-merge commit titles.
2. On each push to `main`, `Release Please` opens or updates `chore(main): release X.Y.Z`, bumping `CHANGELOG.md`, `version.txt`, the workspace version and internal dependency versions in `Cargo.toml`, and `Cargo.lock`. Do not edit `CHANGELOG.md` by hand. Config: `release-please-config.json`, `.release-please-manifest.json`.
3. Merging the PR creates the tag `vX.Y.Z` and a GitHub release.
4. `Publish crates` checks out the tag, verifies it matches the workspace version, runs `cargo package --workspace --locked`, gets a crates.io token through trusted publishing (OIDC) and publishes `witnessed-store`, `witnessed-supervisor` and `witnessed-cli` (binary `witnessed`) in order.
5. `Release binaries` builds `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin` and `x86_64-apple-darwin`, uploads `witnessed-vX.Y.Z-<target>.tar.gz` (binary, `LICENSE`, `README.md`) with a `.sha256`. `install.sh` downloads these and needs no change per release.

`just package` runs the packaging check locally (`just check-ci` includes it); `just clean-package` clears a stale unpacked copy when it fails on a type just added to a workspace crate.

## One-time setup

GitHub App for release-please (a release created by `GITHUB_TOKEN` triggers no other workflow): create an app with Contents, Issues and Pull requests read and write, install it on the repository, set the variable `RELEASE_APP_CLIENT_ID` and the secret `RELEASE_APP_PRIVATE_KEY`, then run the workflow once.

crates.io: trusted publishing is configured per crate under Settings, Trusted Publishing (owner `Druidder-Labs`, repository `witnessed`, workflow `publish-crates.yml`, no environment). The first version of each crate was published by hand with an API token because the setting needs an existing crate. Add the other maintainers as owners (`cargo owner --add`).

## Versioning

Pre-1.0: every release is a GitHub prerelease, `feat` bumps the minor, `fix` the patch, and a breaking change (`feat!` or `BREAKING CHANGE`) bumps the minor. Removing or renaming a command, flag, environment variable, event type or payload field is breaking; adding one is not. The event schema version in `docs/event-model.md` is independent of the crate version.
