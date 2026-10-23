# Agent change assurance: product vision

Agents produce changes. Witnessed verifies the evidence required to trust and approve them.

## Problem

Teams merge code and infrastructure changes written with Claude Code, Codex, Copilot and other agents. Before a merge, a reviewer needs answers that do not depend on the agent's own account:

| Question | Where the answer comes from in this release |
| --- | --- |
| Which commit is under review, and what did it change? | `git` in the verifier's checkout: full commit id, merge base, changed paths, Terraform block types in the changed files |
| Did the required checks run on exactly that commit, and pass? | in-toto test-result attestations in DSSE envelopes, signed by keys the base-branch policy trusts, whose subject is the commit id |
| Who approved, and is the approver independent of the change? | GitHub reviews read by the verifier job through the API: state, reviewer, account type, the commit the review was made on |
| Did the policy itself change in the same pull request? | The policy is read from the base commit; edits to its directory add a requirement for an independent approval |
| What did the agent session record? | A Witnessed bundle: hash chain, anchors, dropped events, the HEAD at session end |
| Can the change be approved? | A Rego policy maps verified facts to requirements; the verifier folds them into `VERIFIED`, `REJECTED` or `INCONCLUSIVE` |

## What it is

- A verifier CLI, `witnessed verify-change`, that runs in CI or on a laptop, offline once the evidence files are present, and prints a deterministic report tied to one commit.
- An evidence model that keeps what was verified (signature, collected from the provider, anchored) apart from what was only declared.
- Standards for the formats: in-toto Statement v1, DSSE, the in-toto test-result predicate, Rego (OPA) for policy.

## What it is not

- Not an agent framework, an observability platform or a SaaS. There is no server.
- Not proof that a change is correct. A passing signed test result proves that the holder of a trusted key reported a pass for that commit; it does not prove that the tests are meaningful.
- Not a monitor of everything an agent did. A session bundle covers the terminal Witnessed supervised and nothing outside it.

## Users

- Platform and security engineers who set the policy and the required check.
- Reviewers who read the report in the pull request summary.
- Auditors who re-run the verification later from the same inputs.

## Principles applied

- Missing evidence never yields approval: no requirement, no attestation, a stale pull request snapshot or a verifier fault each end in `INCONCLUSIVE` or `ERROR`, never `VERIFIED`.
- Integrity is not correctness: the report says what each piece of evidence proves and from which trust source.
- Standards before inventions: the one custom format is the report predicate, documented in `predicates/change-verification-v0.1.md`.
- Minimal data: no prompts, code or review text leave the repository; the pull request snapshot keeps logins, states and commit ids only.
