# Releases

Releases are cut by release-please from Conventional Commits on `main`, and the crates are published to crates.io by a workflow that runs when the GitHub release is published. Nobody runs `cargo publish` by hand after the first release.

## Flow

1. Every commit on `main` follows Conventional Commits (`feat:`, `fix:`, `docs:`, ...). The `Pull request title` workflow enforces it on PR titles, which become the squash-merge commit titles.
2. On each push to `main`, the `Release Please` workflow opens or updates a release pull request titled `chore(main): release X.Y.Z`. It computes the version from the commits since the last tag (`feat` bumps the minor, `fix` the patch; while the version is below 1.0.0 a breaking change bumps the minor, not the major) and updates in that PR:
   - `CHANGELOG.md` (created by the first release PR and extended by every later one; do not edit it by hand and do not add a hand-written file, or release-please nests the old content under the new entry),
   - `version.txt`,
   - `[workspace.package] version` and the two internal dependency versions in `Cargo.toml`,
   - the three workspace entries in `Cargo.lock`.
   The configuration is `release-please-config.json`; `.release-please-manifest.json` records the last released version.
3. Merging that PR creates the tag `vX.Y.Z` and a GitHub release.
4. The `Publish crates` workflow checks out the tag, verifies that the tag matches the workspace version, runs `cargo package --workspace --locked`, obtains a short-lived crates.io token through trusted publishing (OIDC), and runs `cargo publish --workspace --locked`, which publishes `agentrecorder-store`, `agentrecorder-supervisor` and `agentrecorder` in dependency order.

5. The `Release binaries` workflow runs on the same event: one job per target (`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin`, `x86_64-apple-darwin`) builds the CLI, packs `agentrecorder-vX.Y.Z-<target>.tar.gz` with the binary, `LICENSE` and `README.md`, and uploads it with a `.sha256` file through `gh release upload`. `install.sh` at the repository root downloads these; it needs no change per release.

`just package` runs the same packaging check locally; `just check-ci` includes it. When `just package` fails only locally on a type that was just added to a workspace crate, `just clean-package` removes the stale unpacked copy that `cargo package` verifies against.

## One-time setup

### GitHub App for release-please

The release PR must be created with a GitHub App token, not `GITHUB_TOKEN`: a release created by `GITHUB_TOKEN` does not trigger other workflows, so `Publish crates` would never run, and the release PR would get no CI run. Until the app is configured the workflow prints a notice and does nothing.

1. Create a GitHub App on the organization with repository permissions Contents: read and write, Issues: read and write, Pull requests: read and write. No webhook.
2. Install it on `Druidder-Labs/agentrecorder`.
3. In the repository settings, add the variable `RELEASE_APP_CLIENT_ID` (the app's client id) and the secret `RELEASE_APP_PRIVATE_KEY` (a private key generated for the app, PEM contents).
4. Run the `Release Please` workflow once from the Actions tab, or push a commit to `main`.

### crates.io

Trusted publishing can only be configured on a crate that already exists, so the first version of each crate was published once by a maintainer with an API token (`v0.1.0`, 2026-10-21, from the tagged checkout with `CARGO_REGISTRY_TOKEN=... cargo publish --workspace --locked`; the account needs a verified e-mail on crates.io). What remains per crate:

1. On each of the three crates, under Settings, Trusted Publishing, add a GitHub publisher with repository owner `Druidder-Labs`, repository `agentrecorder`, workflow `publish-crates.yml`, and no environment. Until this is done the `Publish crates` workflow fails at the token step with `No Trusted Publishing config found`, and the release has to be published by hand the same way.
2. Add the other maintainers as owners of the three crates (`cargo owner --add`), so publishing does not depend on one account.

## Versioning

The project is pre-1.0. Every release is tagged as a prerelease on GitHub, and a `feat!` or `BREAKING CHANGE` footer bumps the minor version. Removing or renaming a command, a flag, an environment variable, an event type or a payload field is a breaking change; adding one is not. The event schema version in `docs/event-model.md` changes only on incompatible changes to stored events and is independent of the crate version.
