# Agent change assurance: threat model

Scope: `witnessed verify-change` deciding whether one commit in a GitHub pull request may merge, with the example workflow in `examples/assurance/github/`. The session recorder has its own model in `docs/security.md`.

## Trust assumptions

| Component | Trusted for | Why |
| --- | --- | --- |
| The policy at the base commit (`.witnessed/policy/`, read with `--policy-ref`) | Which requirements apply, which keys may attest which checks | Merged earlier through the same controls |
| Keys in `.witnessed/policy/keys/` | Authenticity of attestations | Listed in the trusted policy; the private key lives only in the signing job's environment secret |
| The workflow file on the base branch | Running checks, signing outcomes, running the verifier | `pull_request_target` runs the base-branch definition; the pull request cannot edit it |
| GitHub (API, Actions runners, job results, environments) | Reviews, job outcomes, isolation between jobs | Outside the agent's control; a compromise of GitHub is out of scope |
| `git` in the verify job | Commit ids, changed paths, file contents | Reads objects from the checkout; no pull request code runs in that job |
| The agent, the pull request author, files in the pull request | Nothing | They are the subject of the decision |
| Session bundles | Integrity of the recorded terminal, and with anchors, timing | Self-reported by the person who ran the agent unless anchored |

## Threats and controls

| Threat | Control | Test |
| --- | --- | --- |
| Attestation payload edited | DSSE signature over PAE(payloadType, payload) with P-256; failure is `verifier.evidence-authentic`, `REJECTED` | `an_edited_attestation_is_rejected`, `dsse::tests::a_changed_payload_fails_and_names_the_key` |
| Envelope signed by an unknown key | Only keys in the trusted policy verify | `a_signature_by_an_untrusted_key_is_rejected` |
| Envelope without a signature (no identity) | Unsigned is invalid | `an_unsigned_envelope_carries_no_identity_and_is_rejected` |
| A trusted key used outside its role | Policy maps checks to authorized signers (`attesters`); others do not satisfy requirements | `a_trusted_key_not_authorized_for_checks_does_not_satisfy_them` |
| Attestation from another commit reused, or bundle substituted | Subject `gitCommit` must equal the full commit id; mismatch is `verifier.evidence-commit`, `REJECTED` | `an_attestation_for_another_commit_is_rejected`, `a_substituted_bundle_mixing_valid_and_foreign_evidence_is_rejected` |
| CI result for an older commit | Same subject check; the old result does not satisfy the requirement | same |
| Short or ambiguous commit ids | `--commit` resolves to a full id; subjects must match it exactly; `attest` refuses short ids | `intoto::tests::a_statement_names_a_commit_only_through_the_git_commit_digest` |
| Evidence missing | Each required check without evidence is inconclusive; zero requirements add `verifier.policy-requirements` | `no_evidence_is_inconclusive_never_verified` |
| Required test not run, or failed | Policy requires the named check in `passedTests`; a name in `failedTests` or a `FAILED` result fails | `a_required_check_that_did_not_run_is_inconclusive_and_one_that_failed_rejects` |
| Sensitive change without approval | IAM block types or critical paths require `review.independent-approval` | `an_iam_change_without_approval_is_inconclusive` |
| Approval by the author, a commit author (the agent's account), a bot, or on an older commit | Independence computed from the snapshot: account type `User`, not the author, not a commit author or committer, review `commit_id` equal to the commit | `approvals_by_the_author_a_commit_author_a_bot_or_on_an_old_commit_do_not_count` |
| Policy weakened in the same pull request | Policy read from the base commit; edits to its directory add `verifier.policy-change-approved` even when the policy is read from the head | `a_policy_edited_in_the_change_does_not_judge_it_and_needs_approval` |
| Workflow edited to skip verification | `pull_request_target` runs the base-branch workflow; the policy treats `.github/workflows/` as critical | Workflow design; not testable offline |
| Reviews snapshot from another pull request or repository | Repository must match `--repository` (`REJECTED`); head must equal the commit (`INCONCLUSIVE`) | `a_pull_request_snapshot_for_another_head_or_repository_does_not_approve` |
| Race between approval and a new push | The decision names one commit; GitHub requires the check on the current head; a newer head makes the snapshot stale | same |
| Session bundle edited | Existing hash-chain verification; failure is `verifier.session-integrity`, `REJECTED` | `a_partial_session_for_the_commit_is_inconclusive_and_an_edited_one_rejects` |
| Session with dropped events or no termination record | Reported as partial; the example policy makes a linked partial session inconclusive | same, `dropped_events_and_a_missing_termination_are_reported` |
| Verifier fault read as approval | Git, policy, input and output faults exit 4 with decision `ERROR` | `verifier_failures_are_errors_with_their_own_exit_code` |
| GitHub API unavailable | `collect github-pr` exits 4 and writes nothing | `a_failing_github_query_is_an_error_and_writes_nothing` |
| Policy depends on time, randomness or the network | Those builtins fail the evaluation | `policies_cannot_read_the_clock_the_network_or_randomness` |
| Prompt, code or review text leaks | No prompts or code are read; the snapshot drops review and pull request bodies; the report holds ids, paths, logins and reasons | `a_github_snapshot_is_built_from_api_responses_without_review_text` |

## Residual risks

- The checks job runs pull request code. That code can make its own checks pass, for example by editing a test. The signature proves that CI reported a pass for the commit, not that the tests are meaningful. The policy cannot tell a weakened test from a real one; human review of test changes remains necessary.
- Code that runs inside the checks job shares the runner with that job's later steps. Job outcomes reach the signing job through `needs.<job>.outputs`, which GitHub computes from step outcomes, but a process on the same runner is not isolated from the runner itself.
- The signing key is a long-lived secret in a GitHub environment. Anyone who can change the base-branch workflow, or read the environment's secrets, can sign. Keyless signing with the workflow identity in the certificate removes the stored key (`roadmap.md`).
- With branch protection's required status checks, any workflow that creates a check with the same name satisfies it. The required-workflow rule of a ruleset binds the requirement to the workflow file and ref instead (`github.md`).
- The pull request snapshot is collected, not signed: it is as trustworthy as the job and token that fetched it. Reproducing a decision later uses the same snapshot file; fetching again may show newer reviews.
- `pull_request_target` runs with the base repository's context. The example limits the job that touches pull request files to `permissions: {}` and no secrets, and uses no caches. A change to that workflow needs the same scrutiny as any `pull_request_target` workflow.
- Terraform facts come from a line scan for `resource` and `data` blocks in changed `.tf` files. IAM changes made through modules, `for_each` over a variable, or non-`.tf` inputs are not detected; the scan over-reports when an IAM block is anywhere in a changed file.
- Session bundles without anchors are self-reported. They are shown as `self_reported` and the example policy never requires them.
- Witnessed observes only what it supervised. It does not detect commands run outside the recorded terminal, by other tools, or on other machines.
