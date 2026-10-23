use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_witnessed");
const POLICY: &str = include_str!("../../../examples/assurance/policy/change.rego");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../witnessed-assurance/tests/fixtures");
const ALL_CHECKS: &[&str] = &["terraform-fmt", "terraform-validate", "terraform-security-scan"];

struct Repo {
    dir: tempfile::TempDir,
    base: String,
    head: String,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", "2026-10-23T10:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-10-23T10:00:00Z")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn key(name: &str) -> PathBuf {
    Path::new(FIXTURES).join(name)
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn repo_with(trusted: &[&str], change: impl FnOnce(&Path)) -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let w = dir.path().join("work");
    fs::create_dir(&w).unwrap();
    git(&w, &["init", "-q", "-b", "main"]);
    git(&w, &["config", "user.email", "dev@example.com"]);
    git(&w, &["config", "user.name", "dev"]);
    write(&w.join(".witnessed/policy/change.rego"), POLICY);
    for k in trusted {
        fs::copy(key(&format!("{k}.pub.pem")), {
            let p = w.join(format!(".witnessed/policy/keys/{k}.pub.pem"));
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            p
        })
        .unwrap();
    }
    write(&w.join("infra/main.tf"), "resource \"aws_s3_bucket\" \"logs\" {\n  bucket = \"logs\"\n}\n");
    git(&w, &["add", "-A"]);
    git(&w, &["commit", "-qm", "base"]);
    let base = git(&w, &["rev-parse", "HEAD"]);
    git(&w, &["checkout", "-qb", "agent/iam"]);
    change(&w);
    git(&w, &["add", "-A"]);
    git(&w, &["commit", "-qm", "change"]);
    let head = git(&w, &["rev-parse", "HEAD"]);
    Repo { dir, base, head }
}

fn iam_change(w: &Path) {
    write(
        &w.join("infra/iam.tf"),
        "resource \"aws_iam_role_policy\" \"deploy\" {\n  role   = \"deploy\"\n  policy = data.aws_iam_policy_document.deploy.json\n}\n",
    );
}

fn repo() -> Repo {
    repo_with(&["ci"], iam_change)
}

impl Repo {
    fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn attest(
        &self,
        name: &str,
        commit: &str,
        signer: &str,
        result: &str,
        passed: &[&str],
        failed: &[&str],
    ) -> PathBuf {
        let out = self.path(name);
        let mut args = vec!["attest", "test-result", "--commit", commit, "--result", result, "--keyid", signer];
        for p in passed {
            args.extend(["--passed", p]);
        }
        for f in failed {
            args.extend(["--failed", f]);
        }
        let key = key(&format!("{signer}.key.pem"));
        let key = key.to_str().unwrap();
        args.extend([
            "--key",
            key,
            "--subject-name",
            "git+https://github.com/example/infra",
            "-o",
            out.to_str().unwrap(),
        ]);
        let o = run(&args, &[]);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        out
    }

    fn passing(&self) -> PathBuf {
        self.attest("checks.dsse.json", &self.head, "ci", "passed", ALL_CHECKS, &[])
    }

    fn pull_request(&self, head: &str, repository: &str, reviews: &[(&str, &str, &str, &str)]) -> PathBuf {
        let reviews: Vec<Value> = reviews
            .iter()
            .enumerate()
            .map(|(i, (login, kind, state, commit))| {
                json!({"id": i + 1, "user": {"login": login, "type": kind}, "state": state, "commit_id": commit,
                       "submitted_at": format!("2026-10-23T10:0{i}:00Z")})
            })
            .collect();
        let snap = json!({
            "schema": "witnessed.github-pull-request/v0.1",
            "repository": repository,
            "number": 7,
            "state": "open",
            "author": {"login": "dev", "type": "User"},
            "head_sha": head,
            "base_sha": self.base,
            "base_ref": "main",
            "commits": [{"sha": head, "author_login": "agent-bot", "committer_login": "agent-bot"}],
            "reviews": reviews,
        });
        let p = self.path("pr.json");
        fs::write(&p, snap.to_string()).unwrap();
        p
    }

    fn approved(&self) -> PathBuf {
        self.pull_request(&self.head, "example/infra", &[("alice", "User", "APPROVED", &self.head)])
    }

    fn verify(&self, extra: &[&str]) -> (i32, Value) {
        self.verify_with_policy_ref("main", extra)
    }

    fn verify_with_policy_ref(&self, policy_ref: &str, extra: &[&str]) -> (i32, Value) {
        let work = self.work();
        let mut args = vec![
            "verify-change",
            "--repo",
            work.to_str().unwrap(),
            "--commit",
            &self.head,
            "--base",
            "main",
            "--repository",
            "example/infra",
            "--policy",
            ".witnessed/policy",
            "--policy-ref",
            policy_ref,
            "--format",
            "json",
        ];
        args.extend_from_slice(extra);
        let o = run(&args, &[]);
        let code = o.status.code().unwrap();
        let report: Value = serde_json::from_slice(&o.stdout).unwrap_or_else(|e| {
            panic!("{e}: {}\n{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
        });
        (code, report)
    }
}

fn run(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut c = Command::new(BIN);
    c.args(args).env_remove("GITHUB_REPOSITORY").env_remove("WITNESSED_GH");
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn requirement<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["requirements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no requirement {id} in {report:#}"))
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn an_iam_change_with_signed_checks_and_an_independent_approval_is_verified() {
    let r = repo();
    let (att, pr) = (r.passing(), r.approved());
    let (code, report) = r.verify(&["--attestation", s(&att), "--pull-request", s(&pr)]);
    assert_eq!((code, report["decision"].as_str()), (0, Some("VERIFIED")), "{report:#}");
    assert_eq!(report["change"]["commit"], r.head.as_str());
    assert_eq!(report["policy"]["id"], "terraform-change");
    assert_eq!(report["policy"]["source"]["kind"], "git_tree");
    assert!(report["policy"]["digest"].as_str().unwrap().starts_with("sha256:"));
    assert_eq!(requirement(&report, "check.terraform-security-scan")["status"], "passed");
    assert_eq!(requirement(&report, "review.independent-approval")["status"], "passed");
    assert!(report["reasons"].as_array().unwrap().is_empty());
    let (_, again) = r.verify(&["--attestation", s(&att), "--pull-request", s(&pr)]);
    assert_eq!(report, again, "the same inputs give the same report");
}

#[test]
fn an_iam_change_without_approval_is_inconclusive() {
    let r = repo();
    let att = r.passing();
    let (code, report) = r.verify(&["--attestation", s(&att)]);
    assert_eq!((code, report["decision"].as_str()), (3, Some("INCONCLUSIVE")), "{report:#}");
    let a = requirement(&report, "review.independent-approval");
    assert_eq!(a["status"], "inconclusive");
    assert!(a["reason"].as_str().unwrap().contains("IAM"), "{a}");
    let pr = r.pull_request(&r.head, "example/infra", &[]);
    let (code, _) = r.verify(&["--attestation", s(&att), "--pull-request", s(&pr)]);
    assert_eq!(code, 3);
}

#[test]
fn a_non_iam_terraform_change_needs_no_approval() {
    let r = repo_with(&["ci"], |w| {
        write(&w.join("infra/main.tf"), "resource \"aws_s3_bucket\" \"logs\" {\n  bucket = \"logs-v2\"\n}\n")
    });
    let att = r.attest("a.json", &r.head, "ci", "passed", &["terraform-fmt", "terraform-validate"], &[]);
    let (code, report) = r.verify(&["--attestation", s(&att)]);
    assert_eq!(code, 0, "{report:#}");
    assert!(report["requirements"].as_array().unwrap().iter().all(|q| q["id"] != "review.independent-approval"));
}

#[test]
fn approvals_by_the_author_a_commit_author_a_bot_or_on_an_old_commit_do_not_count() {
    let r = repo();
    let att = r.passing();
    for (who, kind, commit) in [
        ("dev", "User", r.head.as_str()),
        ("agent-bot", "User", r.head.as_str()),
        ("helper", "Bot", r.head.as_str()),
        ("alice", "User", r.base.as_str()),
    ] {
        let pr = r.pull_request(&r.head, "example/infra", &[(who, kind, "APPROVED", commit)]);
        let (code, report) = r.verify(&["--attestation", s(&att), "--pull-request", s(&pr)]);
        assert_eq!(code, 3, "{who} {kind}: {report:#}");
    }
    let pr = r.pull_request(
        &r.head,
        "example/infra",
        &[("alice", "User", "APPROVED", &r.head), ("erin", "User", "CHANGES_REQUESTED", &r.head)],
    );
    let (code, report) = r.verify(&["--attestation", s(&att), "--pull-request", s(&pr)]);
    assert_eq!((code, requirement(&report, "review.no-changes-requested")["status"].as_str()), (1, Some("failed")));
}

#[test]
fn no_evidence_is_inconclusive_never_verified() {
    let r = repo();
    let (code, report) = r.verify(&[]);
    assert_eq!((code, report["decision"].as_str()), (3, Some("INCONCLUSIVE")));
    for c in ALL_CHECKS {
        assert_eq!(requirement(&report, &format!("check.{c}"))["status"], "inconclusive");
    }
    assert!(report["evidence"].as_array().unwrap().is_empty());
}

#[test]
fn a_required_check_that_did_not_run_is_inconclusive_and_one_that_failed_rejects() {
    let r = repo();
    let pr = r.approved();
    let partial = r.attest("partial.json", &r.head, "ci", "passed", &["terraform-fmt", "terraform-validate"], &[]);
    let (code, report) = r.verify(&["--attestation", s(&partial), "--pull-request", s(&pr)]);
    assert_eq!(code, 3, "{report:#}");
    assert_eq!(requirement(&report, "check.terraform-security-scan")["status"], "inconclusive");
    let failed = r.attest(
        "failed.json",
        &r.head,
        "ci",
        "failed",
        &["terraform-fmt", "terraform-security-scan"],
        &["terraform-validate"],
    );
    let (code, report) = r.verify(&["--attestation", s(&failed), "--pull-request", s(&pr)]);
    assert_eq!((code, report["decision"].as_str()), (1, Some("REJECTED")));
    assert_eq!(requirement(&report, "check.terraform-validate")["status"], "failed");
}

#[test]
fn an_edited_attestation_is_rejected() {
    let r = repo();
    let att = r.passing();
    let mut env: Value = serde_json::from_slice(&fs::read(&att).unwrap()).unwrap();
    let payload = String::from_utf8(STANDARD.decode(env["payload"].as_str().unwrap()).unwrap()).unwrap();
    let changed = payload.replace("terraform-fmt", "terraform-fmz");
    assert_ne!(payload, changed);
    env["payload"] = json!(STANDARD.encode(changed));
    fs::write(&att, env.to_string()).unwrap();
    let (code, report) = r.verify(&["--attestation", s(&att), "--pull-request", s(&r.approved())]);
    assert_eq!((code, report["decision"].as_str()), (1, Some("REJECTED")), "{report:#}");
    assert_eq!(requirement(&report, "verifier.evidence-authentic")["status"], "failed");
    assert_eq!(report["evidence"][0]["status"], "invalid");
}

#[test]
fn an_attestation_for_another_commit_is_rejected() {
    let r = repo();
    let old = r.attest("old.json", &r.base, "ci", "passed", ALL_CHECKS, &[]);
    let (code, report) = r.verify(&["--attestation", s(&old), "--pull-request", s(&r.approved())]);
    assert_eq!((code, report["decision"].as_str()), (1, Some("REJECTED")), "{report:#}");
    let q = requirement(&report, "verifier.evidence-commit");
    assert!(q["reason"].as_str().unwrap().contains(&r.head));
    assert_eq!(requirement(&report, "check.terraform-validate")["status"], "inconclusive");
}

#[test]
fn a_substituted_bundle_mixing_valid_and_foreign_evidence_is_rejected() {
    let r = repo();
    let good = r.passing();
    let foreign = r.attest("foreign.json", &"a".repeat(40), "ci", "passed", ALL_CHECKS, &[]);
    let dir = r.path("bundle");
    fs::create_dir(&dir).unwrap();
    fs::copy(&good, dir.join("1.json")).unwrap();
    fs::copy(&foreign, dir.join("2.json")).unwrap();
    let (code, report) = r.verify(&["--attestation", s(&dir), "--pull-request", s(&r.approved())]);
    assert_eq!(code, 1, "{report:#}");
    assert_eq!(report["evidence"].as_array().unwrap().len(), 3);
}

#[test]
fn a_signature_by_an_untrusted_key_is_rejected() {
    let r = repo();
    let att = r.attest("other.json", &r.head, "other", "passed", ALL_CHECKS, &[]);
    let (code, report) = r.verify(&["--attestation", s(&att), "--pull-request", s(&r.approved())]);
    assert_eq!((code, report["decision"].as_str()), (1, Some("REJECTED")), "{report:#}");
    assert!(report["evidence"][0]["detail"].as_str().unwrap().contains("no trusted key"));
}

#[test]
fn an_unsigned_envelope_carries_no_identity_and_is_rejected() {
    let r = repo();
    let att = r.passing();
    let mut env: Value = serde_json::from_slice(&fs::read(&att).unwrap()).unwrap();
    env["signatures"] = json!([]);
    fs::write(&att, env.to_string()).unwrap();
    let (code, report) = r.verify(&["--attestation", s(&att), "--pull-request", s(&r.approved())]);
    assert_eq!(code, 1, "{report:#}");
    assert!(report["evidence"][0]["detail"].as_str().unwrap().contains("no signature"));
}

#[test]
fn a_trusted_key_not_authorized_for_checks_does_not_satisfy_them() {
    let r = repo_with(&["ci", "other"], iam_change);
    let att = r.attest("other.json", &r.head, "other", "passed", ALL_CHECKS, &[]);
    let (code, report) = r.verify(&["--attestation", s(&att), "--pull-request", s(&r.approved())]);
    assert_eq!((code, report["decision"].as_str()), (3, Some("INCONCLUSIVE")), "{report:#}");
    assert_eq!(report["evidence"][0]["status"], "verified");
    let q = requirement(&report, "check.terraform-validate");
    assert!(q["reason"].as_str().unwrap().contains("not authorized"), "{q}");
}

#[test]
fn a_pull_request_snapshot_for_another_head_or_repository_does_not_approve() {
    let r = repo();
    let att = r.passing();
    let newer = "b".repeat(40);
    let pr = r.pull_request(&newer, "example/infra", &[("alice", "User", "APPROVED", &newer)]);
    let (code, report) = r.verify(&["--attestation", s(&att), "--pull-request", s(&pr)]);
    assert_eq!(code, 3, "{report:#}");
    assert!(requirement(&report, "verifier.pull-request-head")["reason"].as_str().unwrap().contains(&newer));
    let pr = r.pull_request(&r.head, "attacker/infra", &[("alice", "User", "APPROVED", &r.head)]);
    let (code, report) = r.verify(&["--attestation", s(&att), "--pull-request", s(&pr)]);
    assert_eq!(
        (code, requirement(&report, "verifier.pull-request-repository")["status"].as_str()),
        (1, Some("failed"))
    );
}

#[test]
fn a_policy_edited_in_the_change_does_not_judge_it_and_needs_approval() {
    let weak = "package witnessed.change\nimport rego.v1\npolicy_id := \"weak\"\npolicy_version := \"9\"\nrequirements contains {\"id\": \"x\", \"status\": \"passed\", \"reason\": \"\"}\n";
    let r = repo_with(&["ci"], |w| {
        iam_change(w);
        write(&w.join(".witnessed/policy/change.rego"), weak);
    });
    let (code, report) = r.verify(&[]);
    assert_eq!(code, 3, "{report:#}");
    assert_eq!(report["policy"]["id"], "terraform-change");
    assert_eq!(report["policy"]["source"]["commit"], r.base.as_str());
    assert_eq!(requirement(&report, "verifier.policy-change-approved")["status"], "inconclusive");
    let (code, report) = r.verify_with_policy_ref(&r.head, &[]);
    assert_eq!((code, report["policy"]["id"].as_str()), (3, Some("weak")), "{report:#}");
    assert_eq!(requirement(&report, "verifier.policy-change-approved")["status"], "inconclusive");
    let att = r.passing();
    let (code, report) = r.verify(&["--attestation", s(&att), "--pull-request", s(&r.approved())]);
    assert_eq!(code, 0, "{report:#}");
    assert_eq!(requirement(&report, "verifier.policy-change-approved")["status"], "passed");
}

#[test]
fn verifier_failures_are_errors_with_their_own_exit_code() {
    let r = repo();
    let work = r.work();
    let base = [
        "verify-change",
        "--repo",
        work.to_str().unwrap(),
        "--base",
        "main",
        "--policy",
        ".witnessed/policy",
        "--format",
        "json",
    ];
    let mut args = base.to_vec();
    args.extend(["--commit", "0123456789abcdef0123456789abcdef01234567", "--policy-ref", "main"]);
    let o = run(&args, &[]);
    assert_eq!(o.status.code(), Some(4));
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["decision"], "ERROR");
    assert!(String::from_utf8_lossy(&o.stderr).contains("not an approval"));
    let mut args = base.to_vec();
    args.extend(["--commit", &r.head, "--policy-ref", "no-such-branch"]);
    assert_eq!(run(&args, &[]).status.code(), Some(4));
    let broken = r.path("broken-policy");
    write(&broken.join("change.rego"), "package witnessed.change\nthis is not rego\n");
    let o = run(
        &[
            "verify-change",
            "--repo",
            work.to_str().unwrap(),
            "--commit",
            &r.head,
            "--base",
            "main",
            "--policy",
            s(&broken),
        ],
        &[],
    );
    assert_eq!(o.status.code(), Some(4));
    let garbage = r.path("garbage.json");
    fs::write(&garbage, "{}").unwrap();
    let mut args = base.to_vec();
    args.extend(["--commit", &r.head, "--policy-ref", "main", "--pull-request", s(&garbage)]);
    assert_eq!(run(&args, &[]).status.code(), Some(4));
}

#[test]
fn a_failing_github_query_is_an_error_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("pr.json");
    let o = run(
        &["collect", "github-pr", "--repository", "example/infra", "--number", "7", "-o", s(&out)],
        &[("WITNESSED_GH", "false")],
    );
    assert_eq!(o.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&o.stderr).contains("nothing was written"));
    assert!(!out.exists());
}

#[test]
fn a_github_snapshot_is_built_from_api_responses_without_review_text() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("gh");
    let head = "c".repeat(40);
    let script = format!(
        "#!/bin/sh\nfor a in \"$@\"; do last=\"$a\"; done\ncase \"$last\" in\n\
         */reviews*) echo '[[{{\"id\":1,\"user\":{{\"login\":\"alice\",\"type\":\"User\"}},\"state\":\"APPROVED\",\"commit_id\":\"{head}\",\"submitted_at\":\"2026-10-23T10:00:00Z\",\"body\":\"secret review text\"}}]]' ;;\n\
         */commits*) echo '[[{{\"sha\":\"{head}\",\"author\":{{\"login\":\"agent-bot\"}},\"committer\":{{\"login\":\"web-flow\"}}}}]]' ;;\n\
         *) echo '{{\"number\":7,\"state\":\"open\",\"user\":{{\"login\":\"dev\",\"type\":\"User\"}},\"head\":{{\"sha\":\"{head}\"}},\"base\":{{\"sha\":\"{head}\",\"ref\":\"main\"}},\"body\":\"pr text\"}}' ;;\nesac\n"
    );
    fs::write(&fake, script).unwrap();
    fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let out = dir.path().join("pr.json");
    let o = run(
        &["collect", "github-pr", "--repository", "example/infra", "--number", "7", "-o", s(&out)],
        &[("WITNESSED_GH", s(&fake))],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let text = fs::read_to_string(&out).unwrap();
    assert!(!text.contains("secret review text") && !text.contains("pr text"), "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        (v["head_sha"].as_str(), v["reviews"][0]["user"]["login"].as_str()),
        (Some(head.as_str()), Some("alice"))
    );
}

#[test]
fn the_report_is_an_in_toto_statement_about_the_commit_and_can_be_signed_and_checked() {
    let r = repo();
    let (att, pr) = (r.passing(), r.approved());
    let work = r.work();
    let statement = r.path("decision.intoto.json");
    let o = run(
        &[
            "verify-change",
            "--repo",
            work.to_str().unwrap(),
            "--commit",
            &r.head,
            "--base",
            "main",
            "--repository",
            "example/infra",
            "--policy",
            ".witnessed/policy",
            "--policy-ref",
            "main",
            "--attestation",
            s(&att),
            "--pull-request",
            s(&pr),
            "--format",
            "intoto",
            "-o",
            s(&statement),
        ],
        &[],
    );
    assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stderr));
    let v: Value = serde_json::from_slice(&fs::read(&statement).unwrap()).unwrap();
    assert_eq!(v["_type"], "https://in-toto.io/Statement/v1");
    assert_eq!(v["subject"][0]["digest"]["gitCommit"], r.head.as_str());
    assert_eq!(v["predicate"]["decision"], "VERIFIED");
    let signed = r.path("decision.dsse.json");
    let key = key("ci.key.pem");
    let o = run(&["attest", "sign", s(&statement), "--key", s(&key), "-o", s(&signed)], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let public = key_path_pub();
    let o = run(&["attest", "verify", s(&signed), "--public-key", s(&public), "--commit", &r.head], &[]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let o = run(&["attest", "verify", s(&signed), "--public-key", s(&public), "--commit", &r.base], &[]);
    assert_eq!(o.status.code(), Some(1));
    let other = Path::new(FIXTURES).join("other.pub.pem");
    assert_eq!(run(&["attest", "verify", s(&signed), "--public-key", s(&other)], &[]).status.code(), Some(1));
}

#[test]
fn attest_refuses_a_short_commit_id_and_a_public_key_as_signing_key() {
    let key = key("ci.key.pem");
    let o = run(&["attest", "test-result", "--commit", "abc1234", "--result", "passed", "--key", s(&key)], &[]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("full 40 or 64 character commit id"));
    let public = key_path_pub();
    let o =
        run(&["attest", "test-result", "--commit", &"a".repeat(40), "--result", "passed", "--key", s(&public)], &[]);
    assert!(String::from_utf8_lossy(&o.stderr).contains("PKCS#8"), "{}", String::from_utf8_lossy(&o.stderr));
}

fn key_path_pub() -> PathBuf {
    Path::new(FIXTURES).join("ci.pub.pem")
}

#[test]
fn text_and_markdown_reports_name_the_decision_and_every_requirement() {
    let r = repo();
    let att = r.passing();
    let work = r.work();
    let summary = r.path("summary.md");
    let o = run(
        &[
            "verify-change",
            "--repo",
            work.to_str().unwrap(),
            "--commit",
            &r.head,
            "--base",
            "main",
            "--policy",
            ".witnessed/policy",
            "--policy-ref",
            "main",
            "--attestation",
            s(&att),
            "--summary",
            s(&summary),
        ],
        &[],
    );
    assert_eq!(o.status.code(), Some(3));
    let text = String::from_utf8(o.stdout).unwrap();
    assert!(text.starts_with("decision:  INCONCLUSIVE\n"), "{text}");
    assert!(text.contains("inconclusive  review.independent-approval"), "{text}");
    let md = fs::read_to_string(&summary).unwrap();
    assert!(
        md.contains("### Agent change verification: INCONCLUSIVE") && md.contains("`check.terraform-validate`"),
        "{md}"
    );
}
