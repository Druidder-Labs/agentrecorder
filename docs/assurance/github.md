# Agent change assurance: GitHub integration

Verifies each pull request head commit and blocks the merge until the decision is `VERIFIED`. No GitHub App and no service: one workflow, one signing key, one policy directory.

## Setup

1. Generate a signing key. Keep the private key out of the repository.

   ```
   openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out ci.key.pem
   openssl ec -in ci.key.pem -pubout -out ci.pub.pem
   ```

2. Add the policy on the default branch: `examples/assurance/policy/change.rego` as `.witnessed/policy/change.rego`, and `ci.pub.pem` as `.witnessed/policy/keys/ci.pub.pem`. Edit `attesters`, `agent_accounts`, `critical_prefixes` and the required checks to fit the repository.
3. Create an environment `assurance-signing` with the secret `WITNESSED_ATTESTATION_KEY` holding the PEM private key, and limit its deployment branches to the default branch. `pull_request_target` runs in the default branch's context and gets the key; a workflow that a pull request adds runs on the pull request's ref and does not. Add a repository variable `WITNESSED_REV` with the full commit id of the witnessed version to install.
4. Copy `examples/assurance/github/agent-change-assurance.yml` to `.github/workflows/` on the default branch. The `checks` job runs `terraform fmt`, `terraform validate` and `checkov` on `infra/`; adjust the steps and the check names together with the policy.
5. Require the result:
   - Recommended: a repository or organization ruleset with "Require workflows to pass before merging" pointing at this workflow file on the default branch. GitHub then matches the workflow, not a job name.
   - Otherwise: branch protection with the required status check `verify` and "Require review from Code Owners", with `.github/` and `.witnessed/` owned by the platform team. A job named `verify` in another workflow would also satisfy a name-only requirement.
6. Add a CODEOWNERS entry for `.witnessed/` so policy changes reach the policy owners.

## What runs

| Job | Pull request code | Secrets | Token permissions | Output |
| --- | --- | --- | --- | --- |
| `checks` | Yes, checked out at the head commit | None | `contents: read` (needed to check out a private repository; not persisted in git config) | Step outcomes |
| `attest` | No checkout | `WITNESSED_ATTESTATION_KEY` (environment) | none | `checks.dsse.json`: a test-result attestation for the head commit |
| `verify` | Checked out, read with git only | None | `contents: read`, `pull-requests: read` | Report in the job summary, `report.json` artifact, exit code |

The workflow uses `pull_request_target`, so its definition comes from the base branch; a pull request that edits it changes nothing until it merges. The `verify` job reads the policy from the base commit (`--policy-ref "${BASE_SHA}"`).

## Key rotation

1. Generate the new key and add `keys/ci-next.pub.pem` and `"ci-next"` to `attesters` in a pull request; it needs an independent approval because it edits the policy.
2. After it merges, replace `WITNESSED_ATTESTATION_KEY` with the new private key and set `--keyid ci-next` in the workflow.
3. Once no open pull request depends on the old key, remove `keys/ci.pub.pem` and `"ci"` from `attesters`.

Attestations signed with a key that is no longer in the policy fail as `verifier.evidence-authentic`; re-run the workflow to sign with the current key.

## After an approval

Reviews submitted after the run are not seen until the `verify` job runs again. Re-run the failed `verify` job from the pull request's Checks tab; it collects reviews again for the same head commit. A new push starts a new run for the new head, and GitHub requires the check on that head.

A `pull_request_review` trigger is not used: it runs the workflow as defined in the pull request, which the pull request could edit.

## Local reproduction

With the report's inputs (the attestations artifact and a snapshot from `collect github-pr`), anyone with a clone runs:

```
witnessed verify-change --commit <head> --base <base> --repository owner/name \
  --policy .witnessed/policy --policy-ref <base> \
  --attestation attestations/ --pull-request pull-request.json --format json
```

With the same inputs and verifier version, the decision, requirements, evidence ids and `input_digest` match the CI report; only `evidence[].label` differs, because it is the local file path. `witnessed attest verify checks.dsse.json --public-key .witnessed/policy/keys/ci.pub.pem --commit <head>` checks one attestation on its own.

## Demo without GitHub

`examples/assurance/demo.sh [path to witnessed]` builds a throwaway repository with an IAM change, signs the checks with a fresh key, and runs three cases: approved (`VERIFIED`, exit 0), not approved (`INCONCLUSIVE`, exit 3), payload edited (`REJECTED`, exit 1). The same cases run in `crates/witnessed-cli/tests/verify_change.rs`.

## GitHub App (designed, not built)

A GitHub App would replace the workflow's re-run step and the name-based check:

- Events: `pull_request` (opened, synchronize), `pull_request_review` (submitted, dismissed), `check_suite` (completed).
- Permissions: `checks: write`, `pull_requests: read`, `contents: read`, `actions: read`.
- On each event it gathers attestations for the head commit (CI artifacts or GitHub attestations), collects reviews, runs the same engine, and writes a check run named by the App, which a ruleset can require from that App only.
- It holds no repository content beyond the run, and its own decision statements can be signed.

It is deferred until the workflow version has users: it adds a hosted service, a private key and an availability dependency to every merge.
