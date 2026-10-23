#!/bin/sh
# Usage: examples/assurance/demo.sh [WITNESSED_BIN]
# Builds a throwaway Terraform repository under a temporary directory, signs CI check results
# with a fresh P-256 key, and runs `witnessed verify-change` twice: an IAM change with a
# reviewer approval (VERIFIED, exit 0) and the same change without one (INCONCLUSIVE, exit 3).
# Needs git and openssl. Writes nothing outside the temporary directory.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
bin=${1:-witnessed}
work=$(mktemp -d "${TMPDIR:-/tmp}/witnessed-demo.XXXXXX")
trap 'rm -rf "$work"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1

openssl ecparam -name prime256v1 -genkey -noout -out "$work/ci.sec1.pem"
openssl pkcs8 -topk8 -nocrypt -in "$work/ci.sec1.pem" -out "$work/ci.key.pem"
openssl ec -in "$work/ci.key.pem" -pubout -out "$work/ci.pub.pem" 2>/dev/null

repo="$work/infra"
mkdir -p "$repo/.witnessed/policy/keys" "$repo/infra"
cp "$here/policy/change.rego" "$repo/.witnessed/policy/"
cp "$work/ci.pub.pem" "$repo/.witnessed/policy/keys/ci.pub.pem"
printf 'resource "aws_s3_bucket" "logs" {\n  bucket = "logs"\n}\n' > "$repo/infra/main.tf"
git -C "$repo" init -q -b main
git -C "$repo" -c user.name=dev -c user.email=dev@example.com add -A
git -C "$repo" -c user.name=dev -c user.email=dev@example.com commit -qm "base infrastructure"

git -C "$repo" checkout -qb agent/deploy-role
printf 'resource "aws_iam_role_policy" "deploy" {\n  role   = "deploy"\n  policy = "{}"\n}\n' > "$repo/infra/iam.tf"
git -C "$repo" -c user.name=agent-bot -c user.email=agent@example.com add -A
git -C "$repo" -c user.name=agent-bot -c user.email=agent@example.com commit -qm "grant the deploy role access"
head=$(git -C "$repo" rev-parse HEAD)
base=$(git -C "$repo" rev-parse main)

mkdir -p "$work/attestations"
"$bin" attest test-result --commit "$head" --subject-name "git+https://github.com/example/infra" \
  --result passed --passed terraform-fmt --passed terraform-validate --passed terraform-security-scan \
  --key "$work/ci.key.pem" --keyid ci -o "$work/attestations/checks.dsse.json"

snapshot() {
  cat > "$work/pr.json" <<EOF
{"schema": "witnessed.github-pull-request/v0.1", "repository": "example/infra", "number": 7, "state": "open",
 "author": {"login": "dev", "type": "User"}, "head_sha": "$head", "base_sha": "$base", "base_ref": "main",
 "commits": [{"sha": "$head", "author_login": "agent-bot", "committer_login": "agent-bot"}],
 "reviews": [$1]}
EOF
}

verify() {
  set +e
  "$bin" verify-change --repo "$repo" --commit "$head" --base main --repository example/infra \
    --policy .witnessed/policy --policy-ref main \
    --attestation "$work/attestations" --pull-request "$work/pr.json"
  code=$?
  set -e
  echo "exit code: $code"
}

echo "== 1. IAM change, checks signed by CI, approved by alice at the head commit"
snapshot "{\"id\": 1, \"user\": {\"login\": \"alice\", \"type\": \"User\"}, \"state\": \"APPROVED\", \"commit_id\": \"$head\", \"submitted_at\": \"2026-10-23T10:00:00Z\"}"
verify

echo
echo "== 2. The same change with no approval"
snapshot ""
verify

echo
echo "== 3. The same change with the attestation's payload edited"
python3 - "$work/attestations/checks.dsse.json" <<'EOF'
import base64, json, sys
p = sys.argv[1]
env = json.load(open(p))
body = base64.b64decode(env["payload"]).replace(b"terraform-fmt", b"terraform-fmz")
env["payload"] = base64.b64encode(body).decode()
json.dump(env, open(p, "w"))
EOF
snapshot "{\"id\": 1, \"user\": {\"login\": \"alice\", \"type\": \"User\"}, \"state\": \"APPROVED\", \"commit_id\": \"$head\", \"submitted_at\": \"2026-10-23T10:00:00Z\"}"
verify
