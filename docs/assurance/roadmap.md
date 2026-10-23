# Agent change assurance: roadmap

Phases from the mission, with what this release delivers and what is left. "Done" means code with tests in `just check`.

## Phase 1: Foundation (done)

- Repository audit and limits (`architecture.md`).
- Contracts: attestation format, pull request snapshot, policy input and contract, report (`evidence-model.md`).
- Threat model and guarantees (`threat-model.md`, `security-guarantees.md`).
- Crate `witnessed-assurance`, independent of the CLI and of GitHub.

## Phase 2: Core verification (done)

- DSSE and in-toto verification with policy keys; commit binding through `gitCommit`.
- Rego policies from a git tree, digest-pinned, without time, randomness or network.
- Verifier requirements for tampered, foreign, stale or partial evidence and for policy edits.
- Three-way decision, `ERROR` with exit 4, deterministic text, JSON, Markdown and in-toto reports.
- Session bundles as evidence, using the existing hash-chain verification.

## Phase 3: GitHub integration (done as a workflow, not run on GitHub)

- `collect github-pr` through `gh api`; reviewer independence facts.
- Example workflow with `pull_request_target`, separated checks, signing and verification jobs; `actionlint` passes.
- Not done: a run on a real repository. The workflow has not executed on GitHub; installation through `cargo install --git` takes minutes per job.

## Phase 4: Attestation (next)

1. Verify Sigstore bundles from GitHub Artifact Attestations and `cosign attest --bundle`, with certificate identity constraints (workflow path and ref from the OIDC claims) in the policy. Removes the long-lived signing key. Needs a TUF trust root and X.509 verification; evaluate `sigstore-rs` against the `deny.toml` license list and the binary size first.
2. Bind the signing workflow to a reusable workflow at a pinned ref (`job_workflow_ref` claim), so a pull request cannot produce a signature with that identity.
3. Sign the report statement in CI and attach it to the commit as a check run or an artifact attestation.

## Phase 5: Production readiness

- Run the example workflow on a demonstration repository with branch rules; record timings.
- Release binaries with `verify-change` so CI installs a checksummed binary instead of compiling.
- Benchmarks for large change sets (thousands of files, many attestations).
- Fuzzing of the DSSE, Statement and snapshot parsers.
- Key rotation: several keys per role in the policy, with a documented overlap period.
- A Terraform plan JSON input (`terraform show -json`) for resource-level facts instead of the block-type scan. The verifier still never runs `plan` itself.
- GitLab merge requests: a snapshot type and a collector; the engine and policy contract stay.

## Not planned

A dashboard, a user system, a tracing backend, an agent framework, scores for "trust", and any decision that depends on a model's judgment.
