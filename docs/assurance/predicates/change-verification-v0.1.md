# Predicate type: change verification v0.1

URI: `https://github.com/Druidder-Labs/witnessed/blob/main/docs/assurance/predicates/change-verification-v0.1.md`

Status: experimental. Fields may change until v1; a change to an existing field's meaning creates a new URI.

## Purpose

Records that a verifier evaluated the evidence for one commit against one policy and reached a decision. It is not build provenance and makes no SLSA claim. Its structure follows the SLSA Verification Summary Attestation: who verified, against which policy, with which inputs, with what result.

## Statement

```json
{"_type": "https://in-toto.io/Statement/v1",
 "subject": [{"name": "owner/name", "digest": {"gitCommit": "<full commit id>"}}],
 "predicateType": "https://github.com/Druidder-Labs/witnessed/blob/main/docs/assurance/predicates/change-verification-v0.1.md",
 "predicate": { ...the report... }}
```

The subject is the commit that was verified. `name` is the repository passed to the verifier, or `commit` when none was.

## Predicate

The report document `witnessed.change-verification/v0.1`, field by field in `../evidence-model.md` (Report). Fields a consumer needs:

| Field | Use |
| --- | --- |
| `decision` | `VERIFIED`, `REJECTED`, `INCONCLUSIVE`. Only `VERIFIED` may be read as approval |
| `verifier.name`, `verifier.version` | Which verifier decided |
| `policy.id`, `policy.version`, `policy.digest`, `policy.source` | Which policy decided; the digest identifies its exact modules and keys |
| `input_digest` | SHA-256 of the policy input; equal digests mean equal evidence |
| `requirements[]` | Each requirement's status, reason and evidence ids |
| `evidence[]` | Each piece of evidence with its id (`sha256:` of the file), status and trust |

## Producing and checking

```
witnessed verify-change ... --format intoto -o decision.intoto.json
witnessed attest sign decision.intoto.json --key ci.key.pem --keyid ci -o decision.dsse.json
witnessed attest verify decision.dsse.json --public-key ci.pub.pem --commit <full commit id>
```

A consumer that trusts the signer still re-checks `subject` against the commit it is about to act on.
