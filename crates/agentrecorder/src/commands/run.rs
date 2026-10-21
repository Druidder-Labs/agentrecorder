use std::fs::OpenOptions;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agentrecorder_store::{Status, Store, kind, now_unix_ns};
use agentrecorder_supervisor::Layout;
use anyhow::{Context, Result, bail};

use agentrecorder_supervisor::client;

use crate::attach::{self, DetachKey};

const READY_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_RETENTION_DAYS: i64 = 30;

#[derive(clap::Args)]
pub struct RunArgs {
    #[arg(short, long, help = "Start the session without attaching; prints the session id")]
    pub detached: bool,
    #[arg(long, help = "Do not record terminal output (replay will be unavailable)")]
    pub no_record: bool,
    #[arg(long, help = "Also record keyboard input; it may contain passwords and secrets")]
    pub record_input: bool,
    #[arg(long, default_value_t = crate::DEFAULT_MAX_RECORD_MB, help = "Stop recording output after this many MiB")]
    pub max_record_mb: u64,
    #[arg(
        long,
        env = "AGENTRECORDER_DETACH_KEY",
        default_value = crate::DEFAULT_DETACH_KEY,
        help = "Key that detaches: ctrl-<key> or none"
    )]
    pub detach_key: String,
    #[arg(
        long,
        help = "Do not add --session-id or hook settings to a Claude Code command; recover --resume then continues the most recent conversation"
    )]
    pub no_agent_integration: bool,
    #[arg(long, help = "Working directory for the command (default: current directory)")]
    pub cwd: Option<PathBuf>,
    #[arg(
        long,
        value_name = "FILE",
        help = "Append hash-chain anchor lines to FILE every 30 s while the session runs (see `agentrecorder anchor`)"
    )]
    pub anchor: Option<PathBuf>,
    #[arg(
        long,
        value_name = "TOKEN",
        env = "AGENTRECORDER_ANCHOR_TOKEN",
        hide_env_values = true,
        help = "Bind the hash chain to a token the reviewer issued; only its SHA-256 is stored, and `verify --token` checks it"
    )]
    pub anchor_token: Option<String>,
    #[arg(trailing_var_arg = true, required = true, value_name = "COMMAND", help = "Command and arguments, after --")]
    pub command: Vec<String>,
}

pub struct Started {
    pub id: String,
    pub finished: bool,
}

pub fn start(layout: &Layout, args: &RunArgs, restarted_from: Option<&str>) -> Result<Started> {
    layout.ensure()?;
    let mut store = Store::open(&layout.db_path())?;
    apply_retention(layout, &mut store);
    let id = new_id(&store)?;
    layout.socket_path(&id)?;
    drop(store);
    let (rows, cols) = attach::terminal_size().unwrap_or((24, 80));
    let exe = std::env::current_exe().context("locating the agentrecorder binary")?;
    let hook_program = exe.to_string_lossy().into_owned();
    let launch =
        crate::adapters::prepare(&args.command, (!args.no_agent_integration).then_some(hook_program.as_str()))?;
    let log = OpenOptions::new().create(true).append(true).open(layout.log_path(&id))?;
    let mut cmd = Command::new(exe);
    cmd.arg("__supervise")
        .args(["--id", &id, "--rows", &rows.to_string(), "--cols", &cols.to_string()])
        .args(["--max-record-bytes", &(args.max_record_mb.saturating_mul(1 << 20)).to_string()]);
    if let Some(cwd) = &args.cwd {
        let cwd = cwd.canonicalize().with_context(|| format!("working directory {}", cwd.display()))?;
        cmd.arg("--cwd").arg(cwd);
    }
    if let Some(mask) = crate::USER_UMASK.get() {
        cmd.args(["--umask", &mask.to_string()]);
    }
    if args.no_record {
        cmd.arg("--no-record");
    }
    if args.record_input {
        cmd.arg("--record-input");
    }
    if let Some(from) = restarted_from {
        cmd.args(["--restarted-from", from]);
    }
    if !args.detached {
        cmd.arg("--hold-output");
    }
    if let Some(agent) = &launch.agent_session_id {
        cmd.args(["--agent-session-id", agent]);
    }
    if let Some(token) = &args.anchor_token {
        if token.trim().is_empty() {
            bail!("--anchor-token needs a non-empty token");
        }
        cmd.args(["--anchor-token-digest", &crate::chain::token_digest(token)]);
    }
    cmd.arg("--").args(&launch.argv);
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::from(log));
    agentrecorder_supervisor::new_session_on_spawn(&mut cmd);
    let mut child = cmd.spawn().context("starting the session supervisor")?;
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if client::is_ready(layout, &id) {
            return Ok(Started { id, finished: false });
        }
        if let Some(st) = child.try_wait()? {
            if let Some(reason) = startup_failure(layout, &id) {
                bail!("session {id} failed to start: {reason}");
            }
            let finished = Store::open(&layout.db_path())?.session(&id)?.is_some_and(|s| s.status == Status::Exited);
            if finished {
                return Ok(Started { id, finished: true });
            }
            let log = last_log_line(layout, &id).unwrap_or_else(|| format!("supervisor exited with {st}"));
            bail!("session {id} failed to start: {log}");
        }
        if Instant::now() > deadline {
            bail!("session {id} did not become ready within {READY_TIMEOUT:?}; see {}", layout.log_path(&id).display());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn startup_failure(layout: &Layout, id: &str) -> Option<String> {
    let store = Store::open(&layout.db_path()).ok()?;
    let mut msg = None;
    let _ = store.for_each_event(id, Some(&[kind::RUNTIME_ERROR]), |e| {
        if let Some(agentrecorder_store::Body::RuntimeError(n)) = e.body()
            && n.code == "spawn_failed"
        {
            msg = Some(n.message);
        }
        Ok(())
    });
    msg
}

fn last_log_line(layout: &Layout, id: &str) -> Option<String> {
    let mut log = String::new();
    std::fs::File::open(layout.log_path(id)).ok()?.read_to_string(&mut log).ok()?;
    log.lines().last().map(str::to_string)
}

pub fn run(layout: &Layout, args: RunArgs) -> Result<i32> {
    let key = DetachKey::parse(&args.detach_key)?;
    let started = start(layout, &args, None)?;
    if let Some(file) = &args.anchor {
        start_anchor(&started.id, file)?;
    }
    let sharing = sharing_note(&args);
    if args.detached {
        println!("{}", started.id);
        if !sharing.is_empty() {
            eprintln!("[agentrecorder] session {}{sharing}", started.id);
        }
        return Ok(0);
    }
    if started.finished {
        return print_finished(layout, &started.id);
    }
    eprintln!(
        "[agentrecorder] session {} started{}{sharing}; detach with {}",
        started.id,
        if args.no_record { " (output not recorded)" } else { " (recording output)" },
        key.label
    );
    match attach::attach(layout, &started.id, &key)? {
        attach::Outcome::Ended => print_finished(layout, &started.id),
        outcome => Ok(attach::report_outcome(&started.id, &outcome)),
    }
}

fn sharing_note(args: &RunArgs) -> String {
    let mut note = String::new();
    if let Some(file) = &args.anchor {
        note.push_str(&format!("; anchors -> {}", file.display()));
    }
    if args.anchor_token.is_some() {
        note.push_str("; chain bound to a token");
    }
    note
}

fn start_anchor(id: &str, file: &PathBuf) -> Result<()> {
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .with_context(|| format!("opening anchor file {}", file.display()))?;
    let exe = std::env::current_exe().context("locating the agentrecorder binary")?;
    Command::new(exe)
        .args(["anchor", id])
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(Stdio::null())
        .spawn()
        .context("starting the anchor writer")?;
    Ok(())
}

fn new_id(store: &Store) -> Result<String> {
    let mut f = std::fs::File::open("/dev/urandom")?;
    for _ in 0..32 {
        let mut b = [0u8; 3];
        f.read_exact(&mut b)?;
        let id = format!("{:02x}{:02x}{:02x}", b[0], b[1], b[2]);
        if !store.id_exists(&id)? {
            return Ok(id);
        }
    }
    bail!("could not allocate a unique session id")
}

fn apply_retention(layout: &Layout, store: &mut Store) {
    let days = std::env::var("AGENTRECORDER_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(DEFAULT_RETENTION_DAYS);
    if days <= 0 {
        return;
    }
    let cutoff = now_unix_ns() - days * 86_400 * 1_000_000_000;
    let _ = super::sessions::prune_before(layout, store, cutoff, false);
}

pub fn print_finished(layout: &Layout, id: &str) -> Result<i32> {
    use std::io::Write;
    let store = Store::open(&layout.db_path())?;
    let mut out = std::io::stdout().lock();
    store.for_each_event(id, Some(&[kind::TERMINAL_OUTPUT]), |e| {
        if let Some(d) = &e.data {
            out.write_all(d)?;
        }
        Ok(())
    })?;
    out.flush()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut s = store.resolve(id)?;
    while s.status != Status::Exited && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        s = store.resolve(id)?;
    }
    if s.status != Status::Exited {
        return Ok(attach::report_outcome(id, &attach::Outcome::Ended));
    }
    let info = client::ExitInfo { exit_code: s.exit_code.map(|c| c as i32), signal: s.exit_signal.map(|c| c as i32) };
    Ok(attach::report_outcome(id, &attach::Outcome::Exited(info)))
}
