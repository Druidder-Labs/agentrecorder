# Agent change assurance: architecture

## Findings from the repository audit (2026-10-23, `d5d8560`)

- Three crates: `witnessed-store` (SQLite events), `witnessed-supervisor` (PTY, recorder, git snapshots), `witnessed-cli` (commands, bundles, `verify`). 231 tests passed before this change.
- Integrity today is a SHA-256 hash chain over session events (`crates/witnessed-cli/src/chain.rs`), checked by `witnessed verify` (`commands/verify.rs`). It proves the bundle files agree; with anchors received while the session ran, it proves the events existed then. There are no signatures and no identities.
- Sessions link to git through `git.snapshot` events: HEAD at start and exit, commits since the start, a diff hash (`supervisor/src/git/snapshot.rs`).
- Hook events (`agent.hook`) and the transcript are self-reported by the agent (`docs/security.md`, Residual risks).
- Nothing in the repository verified a commit against CI results, approvals or a policy.

Limits this places on the design: a bundle is evidence of what the recorded terminal showed, authored by the person who ran the agent. It can support a decision; it cannot be the trust anchor for it. CI results and approvals have to come from systems the agent does not control.

## Components

```
witnessed-assurance (library, no CLI, no provider calls except git)
|-- dsse.rs       DSSE envelope, PAE, P-256 ECDSA sign and verify (RustCrypto p256)
|-- intoto.rs     in-toto Statement v1, ResourceDescriptor, test-result v0.1 predicate
|-- git.rs        commit resolution, merge base, changed paths, policy files at a commit
|-- terraform.rs  resource, data and module block types in a Terraform file
|-- github.rs     pull request snapshot format, reviewer independence facts
|-- policy.rs     policy loading (directory or git tree), digest, Rego evaluation (regorus)
|-- engine.rs     evidence checks, policy input, verifier requirements, decision
`-- report.rs     text, Markdown, in-toto statement, error document

witnessed-cli
|-- commands/verify_change.rs  argument parsing, session bundle adapter, exit codes
|-- commands/attest.rs         attest test-result | sign | verify
`-- commands/collect.rs        collect github-pr (through `gh api`)
```

| Area (mission section 4) | Code |
| --- | --- |
| A. Evidence collection | `collect github-pr`, `attest test-result` in CI, `witnessed bundle` for sessions |
| B. Normalization | `engine.rs` builds the policy input (`witnessed.change-input/v0.1`); `docs/assurance/evidence-model.md` |
| C. Verification | `dsse.rs`, `intoto.rs`, `engine.rs`, `commands/verify.rs` for bundles |
| D. Policy | `policy.rs`, Rego modules in the repository's `.witnessed/policy/` |
| E. Attestation | DSSE + in-toto; report as a Statement with `--format intoto`, signed with `attest sign` |
| F. Integration | `examples/assurance/github/agent-change-assurance.yml`, `docs/assurance/github.md` |

## Flow

```
CI checks job (pull request code, no secrets)
   `-- outcomes ---------> attest job (no pull request code, signing key)
                              `-- checks.dsse.json --+
GitHub API --> collect github-pr --> pull-request.json --+
git (base and head) ------------------------------------+--> verify-change --> report, exit code
policy at the base commit (.witnessed/policy) ----------+                      `--> required check
optional session bundle --------------------------------+
```

`verify-change`:

1. Resolves `--commit` and `--base` to full ids, takes the merge base, lists changed paths (`git diff --name-status -z -M`, no external diff or textconv) and the Terraform block types on both sides of each changed `.tf` file.
2. Loads the policy from `--policy-ref` (a git tree) or a directory. The digest covers every `.rego` module and `keys/*.pub.pem` by name and content.
3. Checks each attestation: DSSE signature against the policy's keys, payload type, Statement type, subject `gitCommit` equal to the commit. Failures become `verifier.evidence-authentic` or `verifier.evidence-commit` (failed).
4. Reads the pull request snapshot: repository must match (`verifier.pull-request-repository`, failed), head must equal the commit (`verifier.pull-request-head`, inconclusive), then derives reviewer independence.
5. Verifies a session bundle with the existing `verify` code; an inconsistent bundle is `verifier.session-integrity` (failed).
6. If the change touches the policy directory, adds `verifier.policy-change-approved`, which passes only with an independent approval at the commit.
7. Evaluates the Rego package `witnessed.change` with only verified evidence in `input.evidence`. Zero policy requirements add `verifier.policy-requirements` (inconclusive).
8. Decides: any failed requirement is `REJECTED`; otherwise any inconclusive one is `INCONCLUSIVE`; otherwise `VERIFIED`.

The engine is independent of the CLI and of GitHub: it takes a `ChangeSet`, a `Policy`, evidence bytes and session facts. GitLab or another provider adds a snapshot type and a collector; the engine and the policy contract stay.

## Exit codes

| Code | Decision | Meaning |
| --- | --- | --- |
| 0 | `VERIFIED` | Every requirement passed |
| 1 | `REJECTED` | A requirement failed: evidence was tampered with, names another commit, a check failed, changes were requested |
| 2 | none | Command-line usage error (clap) |
| 3 | `INCONCLUSIVE` | Evidence missing, not authorized, stale or partial |
| 4 | `ERROR` | The verifier could not decide: git, policy, input or output failure. With `--format json` the document has `"decision": "ERROR"` |

Any code other than 0 blocks a merge. A panic exits 101.

## Choices and trade-offs

- Rego through `regorus` 0.13 (Microsoft, in-process) instead of shelling out to `opa`: one binary, offline, with the `std`, `arc` and `glob` features only. `rand.intn`, `time.now_ns`, `uuid.rfc4122`, `opa.runtime`, `http.send` and `net.lookup_ip_addr` are replaced by extensions that fail the evaluation (`NONDETERMINISTIC` in `policy.rs`, test `policies_cannot_read_the_clock_the_network_or_randomness`), so the same input gives the same decision. The cost: regorus does not implement every OPA builtin; policies must be tested with `witnessed verify-change`, not only `opa test`.
- Keys in the policy directory (P-256 SPKI PEM, the key type cosign generates) instead of keyless Sigstore for this release: verification is offline and testable with fixtures, and no Fulcio, Rekor or TUF client is needed in Rust. Keyless verification is the next step (`roadmap.md`).
- `gh api` for GitHub instead of an HTTP client in Rust: no TLS stack added to the binary, authentication reuses `GH_TOKEN` in Actions.
- One new crate with `p256` and `regorus`; both bring `BSD-3-Clause` code (`subtle`, `regorus`), now allowed in `deny.toml`.
