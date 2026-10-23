use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use witnessed_assurance::dsse::{self, INTOTO_PAYLOAD_TYPE, SigningIdentity, TrustedKey};
use witnessed_assurance::intoto::{
    ResourceDescriptor, STATEMENT_V1, Statement, TEST_RESULT_V0_1, TestOutcome, TestResult,
};

#[derive(clap::Args)]
pub struct AttestArgs {
    #[command(subcommand)]
    pub command: AttestCommand,
}

#[derive(clap::Subcommand)]
pub enum AttestCommand {
    #[command(about = "Write a signed in-toto test-result attestation (DSSE) for a commit")]
    TestResult(TestResultArgs),
    #[command(about = "Sign an in-toto Statement, such as `verify-change --format intoto`, into a DSSE envelope")]
    Sign(SignArgs),
    #[command(about = "Check a DSSE envelope against public keys and print the statement it carries")]
    Verify(CheckArgs),
}

#[derive(clap::Args)]
pub struct KeyArgs {
    #[arg(long, value_name = "FILE", help = "Unencrypted P-256 private key, PKCS#8 PEM")]
    pub key: Option<PathBuf>,
    #[arg(long, value_name = "VAR", conflicts_with = "key", help = "Read the PEM key from this environment variable")]
    pub key_env: Option<String>,
    #[arg(long, default_value = "", help = "Key id hint written into the envelope; verification does not trust it")]
    pub keyid: String,
}

#[derive(Clone, Copy, clap::ValueEnum)]
pub enum Outcome {
    Passed,
    Warned,
    Failed,
}

#[derive(clap::Args)]
pub struct TestResultArgs {
    #[arg(long, help = "Full commit id the tests ran on")]
    pub commit: String,
    #[arg(long, default_value = "", help = "Subject name, such as git+https://github.com/owner/name")]
    pub subject_name: String,
    #[arg(long, value_enum)]
    pub result: Outcome,
    #[arg(long = "passed", value_name = "CHECK", help = "Name of a check that passed; repeatable")]
    pub passed: Vec<String>,
    #[arg(long = "warned", value_name = "CHECK")]
    pub warned: Vec<String>,
    #[arg(long = "failed", value_name = "CHECK")]
    pub failed: Vec<String>,
    #[arg(long, help = "URL of the CI run")]
    pub url: Option<String>,
    #[arg(
        long = "configuration",
        value_name = "PATH",
        help = "Configuration used, such as the workflow file; repeatable"
    )]
    pub configuration: Vec<String>,
    #[command(flatten)]
    pub key: KeyArgs,
    #[arg(long, short, value_name = "FILE", help = "Write the envelope to FILE instead of stdout")]
    pub output: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct SignArgs {
    #[arg(help = "In-toto Statement v1 JSON file")]
    pub statement: PathBuf,
    #[command(flatten)]
    pub key: KeyArgs,
    #[arg(long, short, value_name = "FILE")]
    pub output: Option<PathBuf>,
}

#[derive(clap::Args)]
pub struct CheckArgs {
    #[arg(help = "DSSE envelope JSON file")]
    pub envelope: PathBuf,
    #[arg(
        long = "public-key",
        value_name = "FILE",
        required = true,
        help = "Trusted P-256 public key, PEM; repeatable"
    )]
    pub public_keys: Vec<PathBuf>,
    #[arg(long, help = "Fail unless the statement's subject names this commit")]
    pub commit: Option<String>,
}

pub fn attest(args: &AttestArgs) -> Result<i32> {
    match &args.command {
        AttestCommand::TestResult(a) => test_result(a),
        AttestCommand::Sign(a) => sign(a),
        AttestCommand::Verify(a) => check(a),
    }
}

fn identity(k: &KeyArgs) -> Result<SigningIdentity> {
    let pem = match (&k.key, &k.key_env) {
        (Some(path), _) => fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
        (None, Some(var)) => std::env::var(var).with_context(|| format!("environment variable {var} is not set"))?,
        (None, None) => bail!("pass --key FILE or --key-env VAR"),
    };
    SigningIdentity::from_pem(&k.keyid, &pem).map_err(|e| anyhow!("{e}"))
}

fn write(output: Option<&Path>, statement: &Statement, who: &SigningIdentity) -> Result<i32> {
    let payload = serde_json::to_vec(statement)?;
    let envelope = dsse::sign(INTOTO_PAYLOAD_TYPE, &payload, who);
    let text = serde_json::to_string(&envelope)? + "\n";
    match output {
        Some(path) => fs::write(path, text).with_context(|| format!("writing {}", path.display()))?,
        None => print!("{text}"),
    }
    Ok(0)
}

fn full_commit(s: &str) -> Result<String> {
    if (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(s.to_ascii_lowercase());
    }
    bail!("--commit must be a full 40 or 64 character commit id, not {s:?}")
}

fn test_result(a: &TestResultArgs) -> Result<i32> {
    let commit = full_commit(&a.commit)?;
    let who = identity(&a.key)?;
    let predicate = TestResult {
        result: match a.result {
            Outcome::Passed => TestOutcome::Passed,
            Outcome::Warned => TestOutcome::Warned,
            Outcome::Failed => TestOutcome::Failed,
        },
        configuration: a
            .configuration
            .iter()
            .map(|c| ResourceDescriptor { name: Some(c.clone()), ..ResourceDescriptor::default() })
            .collect(),
        url: a.url.clone(),
        passed_tests: a.passed.clone(),
        warned_tests: a.warned.clone(),
        failed_tests: a.failed.clone(),
    };
    let name = if a.subject_name.is_empty() { "commit" } else { a.subject_name.as_str() };
    let statement = Statement {
        kind: STATEMENT_V1.to_string(),
        subject: vec![ResourceDescriptor::commit(name, &commit)],
        predicate_type: TEST_RESULT_V0_1.to_string(),
        predicate: serde_json::to_value(predicate)?,
    };
    write(a.output.as_deref(), &statement, &who)
}

fn sign(a: &SignArgs) -> Result<i32> {
    let bytes = fs::read(&a.statement).with_context(|| format!("reading {}", a.statement.display()))?;
    let statement = Statement::parse(&bytes).map_err(|e| anyhow!("{}: {e}", a.statement.display()))?;
    write(a.output.as_deref(), &statement, &identity(&a.key)?)
}

fn check(a: &CheckArgs) -> Result<i32> {
    let mut keys = Vec::new();
    for p in &a.public_keys {
        let pem = fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("key");
        let name = name.strip_suffix(".pub.pem").or_else(|| name.strip_suffix(".pem")).unwrap_or(name);
        keys.push(TrustedKey::from_pem(name, &pem).map_err(|e| anyhow!("{}: {e}", p.display()))?);
    }
    let bytes = fs::read(&a.envelope).with_context(|| format!("reading {}", a.envelope.display()))?;
    let verified = match dsse::parse(&bytes).and_then(|env| dsse::verify(&env, &keys)) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("witnessed: {}: {e}", a.envelope.display());
            return Ok(1);
        }
    };
    let statement = match Statement::parse(&verified.payload) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("witnessed: {}: signed by {}, but {e}", a.envelope.display(), verified.signer);
            return Ok(1);
        }
    };
    if let Some(c) = &a.commit
        && !statement.names_commit(c)
    {
        eprintln!(
            "witnessed: {}: signed by {}, but its subject names {:?}, not {c}",
            a.envelope.display(),
            verified.signer,
            statement.subject_commits()
        );
        return Ok(1);
    }
    eprintln!("witnessed: signature by {} verified; payload type {}", verified.signer, verified.payload_type);
    println!("{}", serde_json::to_string_pretty(&statement)?);
    Ok(0)
}
