# Agent change assurance: standards

One row per standard the mission names: the problem it solves, how this release uses it, what that guarantees, and what stays out. "Adopted" means code in this release; "designed" means an interface or document only; "deferred" means not used yet, with the reason.

| Standard | Status | Problem it solves here | Integration | Guarantee | Limits and out of scope |
| --- | --- | --- | --- | --- | --- |
| in-toto Attestation Framework, Statement v1 | Adopted | A common envelope for "this claim is about this commit" | `intoto.rs`; subjects use the `gitCommit` digest | The claim names one commit by its full id | Only `gitCommit` subjects bind to a change; artifact digests are not interpreted |
| in-toto test-result predicate v0.1 | Adopted | Reporting check outcomes without a custom format | `attest test-result`; the policy reads `passedTests`, `failedTests`, `result` | The signer reported these outcomes | Says nothing about test quality |
| DSSE v1 | Adopted | Signing the exact payload bytes and type | `dsse.rs`, PAE per the specification, P-256 ECDSA via RustCrypto `p256` | Payload and type unchanged since signing by the key holder | Key distribution is the policy directory; no revocation or expiry |
| Sigstore (Fulcio, Rekor, bundles) | Deferred | Keyless signing bound to a workflow identity, public transparency | Next step: verify Sigstore bundles from GitHub Artifact Attestations (`roadmap.md`) | none in this release | Needs a TUF trust root and X.509 chain verification; adding them to the verifier is its own change. Public keys in the policy use the same P-256 type cosign generates |
| SLSA Build provenance | Not used for this purpose | Proves how an artifact was built | none; a change verification is not build provenance and is not labeled as SLSA | none | A future policy may require SLSA provenance for artifacts a change produces |
| SLSA Verification Summary Attestation | Design reference | Shape of a "verifier X checked Y against policy Z" record | The report predicate follows its structure: verifier, policy digest, input digests, result | none beyond the report | Not emitted as a VSA, since `verifiedLevels` are SLSA levels |
| Open Policy Agent / Rego | Adopted | Policies in a language teams already review and test | `policy.rs` with `regorus` 0.13, Rego v1, package `witnessed.change` | Same input, same requirements; no time, randomness or network | `regorus` is not OPA; use builtins `witnessed verify-change` accepts |
| GitHub Checks, required checks, rulesets | Adopted through a workflow | Blocking a merge on the decision | The `verify` job's status; exit code is the gate | GitHub enforces it on the head commit | Name-based required checks can be satisfied by another job with the same name; prefer a ruleset's required workflow |
| GitHub Artifact Attestations | Deferred | GitHub-signed provenance for artifacts | Same path as Sigstore | none in this release | `actions/attest` subjects are artifact digests, not commits |
| OpenID Connect | Deferred | Workload identity of the CI job | Arrives with keyless Sigstore (the certificate carries the OIDC claims) | none in this release | Not used to authenticate GitHub API calls; `gh` uses `GITHUB_TOKEN` |
| SPIFFE/SPIRE | Not needed | Workload identity inside a cluster | none | none | The MVP runs on GitHub-hosted runners, where GitHub's OIDC token is the workload identity |
| OpenTelemetry GenAI semantic conventions | Deferred | Agent telemetry as spans | none | none | Telemetry is self-reported by the agent's process; it would enter as `declared`, never as a verified fact |
| OWASP Agent Control Standard | Not reviewed | Controls for agent permissions and oversight | none | none | Its control list was not read for this release, so no mapping or conformance is claimed; a mapping needs the published control list |
| SCITT (RFC 9943 per the mission) | Deferred | Append-only, verifiable registration of statements | A transparency service could countersign the signed report statement | none | No public log in this release; reports are files and CI artifacts |

## Minimum set for this release

in-toto Statement v1, the test-result predicate, DSSE with P-256 keys, Rego, and GitHub's required checks. That set authenticates CI results for one commit, decides offline, and blocks a merge. Everything else in the table either needs infrastructure (Sigstore, SCITT), addresses another problem (SLSA build provenance, SPIFFE), or would add self-reported data (OpenTelemetry).
