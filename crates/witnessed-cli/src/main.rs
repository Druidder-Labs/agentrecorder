mod adapters;
mod agent_state;
mod attach;
mod bundle;
mod chain;
mod commands;
mod decisions;
mod fmt;
mod outcome;
mod procinfo;
mod render;
mod sanitize;
mod state;
mod transcript;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use witnessed_supervisor::Layout;

pub const SESSION_HELP: &str = "Session id, a unique prefix of it, or `last` for the most recent session";
pub const DEFAULT_DETACH_KEY: &str = "ctrl-\\";
pub const DEFAULT_MAX_RECORD_MB: u64 = 256;
pub static USER_UMASK: std::sync::OnceLock<nix::libc::mode_t> = std::sync::OnceLock::new();

#[derive(Parser)]
#[command(
    name = "witnessed",
    version,
    arg_required_else_help = true,
    about = "The verifiable record of AI-assisted work",
    after_help = "Start here:\n  witnessed run -- claude     record a session\n  witnessed review            walk through your decisions\n  witnessed view              open it in the browser\n  witnessed bundle --zip      package it for a reviewer\n\nSessions default to the last one; pass an id or a prefix for another."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Record a command, such as a coding agent, in a supervised terminal")]
    Run(commands::run::RunArgs),
    #[command(about = "Walk through the moments you approved, declined, stopped or corrected the agent")]
    Review(commands::review::ReviewArgs),
    #[command(about = "Open a session or a bundle in the browser")]
    View(commands::view::ViewArgs),
    #[command(about = "Package a finished session for a reviewer")]
    Bundle(commands::bundle::BundleArgs),
    #[command(about = "Check that a bundle was not edited")]
    Verify(commands::verify::VerifyArgs),
    #[command(about = "List sessions", visible_alias = "ls")]
    Sessions(commands::sessions::SessionsArgs),
    #[command(about = "Reconnect this terminal to a running session")]
    Attach(commands::sessions::AttachArgs),
    #[command(about = "Disconnect whichever terminal is attached to a session")]
    Detach(commands::sessions::DetachArgs),
    #[command(about = "Play a session back in this terminal")]
    Replay(commands::replay::ReplayArgs),
    #[command(about = "Show what was measured in a session: processes, git, tool calls, tokens")]
    Inspect(commands::inspect::InspectArgs),
    #[command(about = "Reattach, restart or resume a session whose terminal is gone")]
    Recover(commands::recover::RecoverArgs),
    #[command(about = "Mark a moment in a running session; replay --from-mark starts there")]
    Mark(commands::sessions::MarkArgs),
    #[command(about = "Stop a running session")]
    Stop(commands::sessions::StopArgs),
    #[command(about = "Export events as JSON Lines or an asciicast file")]
    Export(commands::export::ExportArgs),
    #[command(about = "Stream hash-chain anchors to a reviewer while a session runs")]
    Anchor(commands::anchor::AnchorArgs),
    #[command(about = "Delete finished sessions")]
    Rm(commands::sessions::RmArgs),
    #[command(about = "Delete finished sessions older than an age")]
    Prune(commands::sessions::PruneArgs),
    #[command(about = "Print a shell completion script: bash, zsh, fish or elvish")]
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    #[command(
        about = "Record an agent hook event (JSON on stdin) in the session named by WITNESSED_SESSION",
        hide = true
    )]
    Hook(commands::hook::HookArgs),
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
    let _ = USER_UMASK.set(previous.bits());
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("witnessed: error: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn dispatch(cli: Cli) -> anyhow::Result<i32> {
    match cli.command {
        Command::Hook(args) => return Ok(commands::hook::hook(args)),
        Command::Verify(args) => return commands::verify::verify(&args),
        Command::Completions { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(shell, &mut Cli::command(), "witnessed", &mut std::io::stdout());
            return Ok(0);
        }
        _ => {}
    }
    let layout = Layout::from_env()?;
    if !matches!(cli.command, Command::Supervise(_)) {
        witnessed_supervisor::default_sigpipe();
    }
    match cli.command {
        Command::Run(args) => commands::run::run(&layout, args),
        Command::Sessions(args) => commands::sessions::list(&layout, &args),
        Command::Attach(args) => commands::sessions::attach(&layout, &args),
        Command::Detach(args) => commands::sessions::detach(&layout, &args),
        Command::Mark(args) => commands::sessions::mark(&layout, &args),
        Command::Inspect(args) => commands::inspect::inspect(&layout, &args),
        Command::Recover(args) => commands::recover::recover(&layout, args),
        Command::Replay(args) => commands::replay::replay(&layout, args),
        Command::Stop(args) => commands::sessions::stop(&layout, &args),
        Command::Review(args) => commands::review::review(&layout, args),
        Command::Export(args) => commands::export::export(&layout, args),
        Command::Bundle(args) => commands::bundle::bundle(&layout, &args),
        Command::Anchor(args) => commands::anchor::anchor(&layout, &args),
        Command::View(args) => commands::view::view(&layout, &args),
        Command::Rm(args) => commands::sessions::remove(&layout, &args),
        Command::Prune(args) => commands::sessions::prune(&layout, &args),
        Command::Hook(_) | Command::Verify(_) | Command::Completions { .. } => {
            unreachable!("handled before the state directory is resolved")
        }
        Command::Supervise(a) => {
            layout.ensure()?;
            witnessed_supervisor::run(witnessed_supervisor::Config {
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
