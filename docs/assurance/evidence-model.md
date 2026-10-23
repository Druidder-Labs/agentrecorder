# Agent change assurance: evidence model

Four documents, each versioned in its own `schema` or type field. Adding a field keeps the version; removing, renaming or changing the meaning of one increments it.

## Attestation: in-toto Statement v1 in a DSSE envelope

Written by `witnessed attest test-result` or any tool that emits the same format.

```json
{"payloadType": "application/vnd.in-toto+json",
 "payload": "<base64 of the statement>",
 "signatures": [{"keyid": "ci", "sig": "<base64 DER ECDSA P-256 signature over PAE>"}]}
```

```json
{"_type": "https://in-toto.io/Statement/v1",
 "subject": [{"name": "git+https://github.com/example/infra",
              "digest": {"gitCommit": "<full commit id>"}}],
 "predicateType": "https://in-toto.io/attestation/test-result/v0.1",
 "predicate": {"result": "PASSED",
               "configuration": [{"name": ".github/workflows/agent-change-assurance.yml"}],
               "url": "https://github.com/example/infra/actions/runs/1/attempts/1",
               "passedTests": ["terraform-fmt", "terraform-validate"],
               "warnedTests": [], "failedTests": []}}
```

- `keyid` is a hint and is not trusted; the verifier tries every key in the policy and records the name of the key that verified.
- The subject binds the result to a commit through the `gitCommit` digest; `name` is informational.
- The predicate is the in-toto test-result predicate, unchanged. A check is identified by its name in `passedTests`, `warnedTests` or `failedTests`.
- Other predicate types verify and reach the policy unchanged; a test-result predicate that does not parse is invalid evidence.

## Pull request snapshot: `witnessed.github-pull-request/v0.1`

Written by `witnessed collect github-pr` from `GET /repos/{o}/{r}/pulls/{n}`, `/reviews` and `/commits` (paginated). Bodies, titles and review text are dropped.

| Field | Content |
| --- | --- |
| `repository`, `number`, `state` | As collected |
| `author` | `login`, `type` of the pull request author |
| `head_sha`, `base_sha`, `base_ref` | At collection time |
| `commits[]` | `sha`, `author_login`, `committer_login` (GitHub accounts linked to the commit, when any) |
| `reviews[]` | `id`, `user` (`login`, `type`), `state`, `commit_id`, `submitted_at` |

## Policy input: `witnessed.change-input/v0.1`

What the Rego package `witnessed.change` receives as `input`. `evidence` holds only what verified; `declared` holds values the verifier took from its caller without checking.

```
change
  repository           from --repository (also under declared)
  commit, base, merge_base
  files[]              path, status (A, M, D, R, C, T), old_path for renames and copies
  terraform[]          path, block_types: resource types and data.<type> on either side of the change
evidence
  attestations[]       id (sha256 of the envelope), label, signer (trusted key name), trust: "signature",
                       predicate_type, subjects, predicate
  pull_request         null, or: id, repository, number, state, author, head_sha, base_sha,
                       head_matches_commit, commit_authors, reviewers[], independent_approvals[],
                       changes_requested[]
  sessions[]           id, label, session_id, program, anchored, anchored_through_seq, token_matched,
                       exit_head, commits, dropped_events, terminated, trust ("anchored" or
                       "self_reported"), linked_to_commit, partial
declared
  repository
```

Reviewer fields: `login`, `account_type`, `state` (the latest of `APPROVED`, `CHANGES_REQUESTED`, `DISMISSED`; comments do not replace it), `commit_id`, `at_commit`, `is_author`, `is_commit_author`, `independent` (account type `User`, not the author, not a commit author or committer). `independent_approvals` lists independent reviewers whose latest state is `APPROVED` at the commit.

Attestations that fail verification, attestations for another commit, a snapshot for another repository or head, and inconsistent session bundles never appear under `evidence`; the verifier adds its own requirement for them.

## Policy contract

Package `witnessed.change`, Rego v1. Rules:

| Rule | Type | Required |
| --- | --- | --- |
| `policy_id` | string | yes |
| `policy_version` | string | yes |
| `requirements` | set of objects | yes; an empty set is allowed and makes the result inconclusive |

A requirement is `{"id": string, "status": "passed" | "failed" | "inconclusive", "reason": string, "evidence": [evidence ids]}`. Ids starting with `verifier.` are reserved. A duplicate id, another status or a missing field is a policy error (exit 4).

Policy directory layout: `*.rego` modules (`*_test.rego` skipped) and `keys/<name>.pub.pem`. Other files are ignored and not part of the digest.

## Report: `witnessed.change-verification/v0.1`

`--format json`. `--format intoto` wraps the same object as the predicate of an in-toto Statement whose subject is the commit; the predicate type is defined in `predicates/change-verification-v0.1.md`.

| Field | Content |
| --- | --- |
| `decision` | `VERIFIED`, `REJECTED`, `INCONCLUSIVE` |
| `verifier` | `name`, `version` |
| `change` | `repository`, `commit`, `base`, `merge_base`, `files` |
| `policy` | `id`, `version`, `digest` (`sha256:`), `source` (`git_tree` with `commit` and `path`, or `directory` with `path`) |
| `input_digest` | SHA-256 of the policy input |
| `requirements[]` | `id`, `status`, `reason`, `evidence`, `source` (`policy` or `verifier`), sorted by id |
| `evidence[]` | `id`, `kind` (`attestation`, `pull_request`, `session`), `label`, `status`, `trust`, `signer`, `predicate_type`, `detail` |
| `reasons[]` | One line per requirement that did not pass |

Evidence status values: attestation `verified`, `invalid`, `other_commit`; pull request `collected`, `stale`, `other_repository`; session `consistent`, `invalid`. Trust values: `signature`, `collected`, `anchored`, `self_reported`, `none`.

An `ERROR` with `--format json` writes `{"schema", "decision": "ERROR", "exit_code": 4, "verifier", "commit", "error"}`.
