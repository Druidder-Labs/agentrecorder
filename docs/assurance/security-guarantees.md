# Agent change assurance: security guarantees

What a decision means, stated as narrowly as the code supports. Assumptions are in `threat-model.md`.

## `VERIFIED` means

1. The commit is the full id printed in `change.commit`; the changed paths are those between it and the merge base with `change.base`.
2. The policy that decided is the one whose digest is in `policy.digest`, read from `policy.source`. With `--policy-ref`, that is a git tree at a commit, not the work tree.
3. Every attestation presented verified against a key in that policy, has payload type `application/vnd.in-toto+json`, is an in-toto Statement v1, and names the commit in a `gitCommit` subject digest.
4. When a pull request snapshot was presented, it is for the repository given in `--repository` and its head is the commit.
5. Every requirement the policy returned has status `passed`, and the policy returned at least one.
6. If the change touched the policy directory, an independent approval at the commit exists.

## `VERIFIED` does not mean

- That the change is correct, safe or what the requester wanted.
- That the tests are meaningful or were not weakened in the same change.
- That the reviewer read the change.
- That the agent did nothing outside the recorded session, or that a session exists at all, unless the policy requires one.
- That the key holder was not compromised, or that GitHub returned truthful data.

## `REJECTED` and `INCONCLUSIVE`

- `REJECTED`: at least one requirement failed. The verifier's own failures are tampered or unauthenticated evidence, evidence for another commit, a snapshot for another repository, and an inconsistent session bundle. The example policy adds failed checks and requested changes.
- `INCONCLUSIVE`: nothing failed, but at least one requirement lacks trustworthy evidence: a check without an authorized attestation, a missing approval, a stale snapshot, a partial session, or a policy that required nothing.

Neither allows a merge when the check is required.

## `ERROR`

The verifier could not reach a decision: the commit or base does not resolve, the policy is missing or does not compile, a policy rule returned an unexpected shape, a builtin was refused, an input is unreadable, malformed or over 16 MiB, `--pull-request` was given without `--repository`, `--policy` with `--policy-ref` leaves the repository, or the report could not be written. With `--summary`, the summary gets an `ERROR` section. Exit code 4, never 0.

## Determinism

Given the same repository objects, policy, evidence files and verifier version, the report is byte-identical: no timestamps, sorted requirements and evidence, sorted JSON keys, and no policy access to time, randomness or the network. `input_digest` is the SHA-256 of the exact policy input, so two runs can be compared without the files. Test: `an_iam_change_with_signed_checks_and_an_independent_approval_is_verified`.

## Cryptography

No algorithm is implemented here. Signatures use ECDSA P-256 with SHA-256 from RustCrypto `p256` 0.14 (`ecdsa` 0.17), over the DSSE v1 PAE encoding. Digests use `sha2` 0.11. A fixture envelope signed with the OpenSSL CLI (`tests/fixtures/openssl-signed.dsse.json`) verifies, so signatures interoperate with an independent implementation. Keys are PEM: SubjectPublicKeyInfo for public keys, unencrypted PKCS#8 for private keys.

The fixture private keys in `crates/witnessed-assurance/tests/fixtures/` are test keys, public by design; never trust them in a policy.
