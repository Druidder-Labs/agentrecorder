mod adapters;
mod agent_state;
mod attach;
mod chain;
mod commands;
mod fmt;
mod procinfo;
mod render;
mod sanitize;
mod state;
mod transcript;

use std::path::PathBuf;
use std::process::ExitCode;

use agentrecorder_supervisor::Layout;
use clap::{Parser, Subcommand};

pub const SESSION_HELP: &str = "Session id, a unique prefix of it, or `last` for the most recent session";
pub const DEFAULT_DETACH_KEY: &str = "ctrl-\\";
pub const DEFAULT_MAX_RECORD_MB: u64 = 256;
pub static USER_UMASK: std::sync::OnceLock<u32> = std::sync::OnceLock::new();

#[derive(Parser)]
#[command(
    name = "agentrecorder",
    version,
    arg_required_else_help = true,
    about = "Run, inspect, recover and replay terminal sessions of AI coding agents and other programs",
    after_help = "Examples:\n  agentrecorder run -- claude\n  agentrecorder sessions\n  agentrecorder attach a91f2c\n  agentrecorder inspect a91f2c\n  agentrecorder replay last --speed 2\n  agentrecorder bundle last\n\nState lives in $AGENTRECORDER_HOME (default: $XDG_STATE_HOME/agentrecorder or ~/.local/state/agentrecorder)."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Start a command in a new supervised PTY session and attach to it")]
    Run(commands::run::RunArgs),
    #[command(about = "List sessions", visible_alias = "ls")]
    Sessions {
        #[arg(long, help = "Print JSON instead of a table")]
        json: bool,
    },
    #[command(about = "Attach the current terminal to a running session")]
    Attach {
        #[arg(help = crate::SESSION_HELP)]
        session: String,
        #[arg(
            long,
            env = "AGENTRECORDER_DETACH_KEY",
            default_value = DEFAULT_DETACH_KEY,
            help = "Key that detaches: ctrl-<key> or none"
        )]
        detach_key: String,
    },
    #[command(about = "Detach whichever client is attached to a session")]
    Detach {
        #[arg(help = crate::SESSION_HELP)]
        session: String,
    },
    #[command(about = "Record a marker in a running session; replay --from-mark starts there")]
    Mark {
        #[arg(help = crate::SESSION_HELP)]
        session: String,
        #[arg(default_value = "", help = "Label shown by inspect, replay and asciicast export")]
        label: String,
    },
    #[command(about = "Show measured process, terminal and event data for a session")]
    Inspect {
        #[arg(help = crate::SESSION_HELP)]
        session: String,
        #[arg(long, help = "Print JSON instead of text")]
        json: bool,
        #[arg(long, help = "Show the text of each prompt read from the agent transcript (hidden by default)")]
        prompts: bool,
    },
    #[command(about = "Reattach, or explain what can and cannot be recovered")]
    Recover(commands::recover::RecoverArgs),
    #[command(about = "Play back recorded terminal output without re-executing anything")]
    Replay(commands::replay::ReplayArgs),
    #[command(about = "Stop the managed process of a session")]
    Stop {
        #[arg(help = crate::SESSION_HELP)]
        session: String,
        #[arg(long, default_value = "HUP", help = "First signal: HUP, TERM, INT or KILL")]
        signal: String,
        #[arg(long, default_value_t = 5.0, help = "Seconds before escalating to SIGKILL")]
        timeout: f64,
    },
    #[command(about = "Export recorded events as JSON Lines or an asciicast v2 or v3 file")]
    Export(commands::export::ExportArgs),
    #[command(about = "Write a shareable directory: dashboard, asciicast, events, hash chain and manifest")]
    Bundle(commands::bundle::BundleArgs),
    #[command(about = "Print hash-chain anchors of a session while it runs, for a third party to keep")]
    Anchor(commands::bundle::AnchorArgs),
    #[command(about = "Check a bundle's files and hash chain, and optionally the anchors received for it")]
    Verify(commands::bundle::VerifyArgs),
    #[command(about = "Open the dashboard of a bundle or of a finished session in a browser")]
    View(commands::bundle::ViewArgs),
    #[command(about = "Delete finished sessions and their recordings")]
    Rm {
        #[arg(required = true, help = "Session ids or unique prefixes")]
        sessions: Vec<String>,
    },
    #[command(about = "Delete finished sessions older than a given age")]
    Prune {
        #[arg(long, default_value = "30d", help = "Age such as 12h, 7d or 90m")]
        older_than: String,
    },
    #[command(about = "Record an agent hook event (JSON on stdin) in the session named by AGENTRECORDER_SESSION")]
    Hook(commands::hook::HookArgs),
    #[command(about = "Print a shell completion script: bash, zsh, fish or elvish")]
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    #[command(name = "__supervise", hide = true)]
    Supervise(SuperviseArgs),
}

#[derive(clap::Args)]
struct SuperviseArgs {
    #[arg(long)]
    id: String,
    #[arg(long)]
    rows: u16,
    #[arg(long)]
    cols: u16,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    no_record: bool,
    #[arg(long)]
    record_input: bool,
    #[arg(long)]
    max_record_bytes: u64,
    #[arg(long)]
    umask: Option<u32>,
    #[arg(long)]
    restarted_from: Option<String>,
    #[arg(long)]
    agent_session_id: Option<String>,
    #[arg(long)]
    anchor_token_digest: Option<String>,
    #[arg(long)]
    hold_output: bool,
    #[arg(last = true, required = true)]
    argv: Vec<String>,
}

fn main() -> ExitCode {
    let previous = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
    let _ = USER_UMASK.set(previous.bits() as u32);
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("agentrecorder: error: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(cli: Cli) -> anyhow::Result<i32> {
    match cli.command {
        Command::Hook(args) => return Ok(commands::hook::hook(args)),
        Command::Verify(args) => return commands::bundle::verify(args),
        Command::Completions { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(shell, &mut Cli::command(), "agentrecorder", &mut std::io::stdout());
            return Ok(0);
        }
        _ => {}
    }
    let layout = Layout::from_env()?;
    if !matches!(cli.command, Command::Supervise(_)) {
        agentrecorder_supervisor::default_sigpipe();
    }
    match cli.command {
        Command::Run(args) => commands::run::run(&layout, args),
        Command::Sessions { json } => commands::sessions::list(&layout, json),
        Command::Attach { session, detach_key } => commands::sessions::attach(&layout, &session, &detach_key),
        Command::Detach { session } => commands::sessions::detach(&layout, &session),
        Command::Mark { session, label } => commands::sessions::mark(&layout, &session, &label),
        Command::Inspect { session, json, prompts } => commands::inspect::inspect(&layout, &session, json, prompts),
        Command::Recover(args) => commands::recover::recover(&layout, args),
        Command::Replay(args) => commands::replay::replay(&layout, args),
        Command::Stop { session, signal, timeout } => commands::sessions::stop(&layout, &session, &signal, timeout),
        Command::Export(args) => commands::export::export(&layout, args),
        Command::Bundle(args) => commands::bundle::bundle(&layout, args),
        Command::Anchor(args) => commands::bundle::anchor(&layout, args),
        Command::View(args) => commands::bundle::view(&layout, args),
        Command::Rm { sessions } => commands::sessions::remove(&layout, &sessions),
        Command::Prune { older_than } => commands::sessions::prune(&layout, &older_than),
        Command::Hook(_) | Command::Verify(_) | Command::Completions { .. } => {
            unreachable!("handled before the state directory is resolved")
        }
        Command::Supervise(a) => {
            layout.ensure()?;
            agentrecorder_supervisor::run(agentrecorder_supervisor::Config {
                layout,
                id: a.id,
                argv: a.argv,
                cwd: a.cwd,
                rows: a.rows,
                cols: a.cols,
                record_output: !a.no_record,
                record_input: a.record_input,
                max_record_bytes: a.max_record_bytes,
                restarted_from: a.restarted_from,
                agent_session_id: a.agent_session_id,
                anchor_token_digest: a.anchor_token_digest,
                hold_output: a.hold_output,
                umask: a.umask,
            })
        }
    }
}
