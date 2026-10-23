use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::json;
use witnessed_store::{format_rfc3339, now_unix_ns};
use witnessed_supervisor::Layout;

use super::{Follow, open_store};
use crate::chain::{self, Hash};

#[derive(clap::Args)]
pub struct AnchorArgs {
    #[arg(help = crate::SESSION_HELP)]
    pub session: String,
    #[arg(long, default_value_t = 30, help = "Seconds between anchors while the session runs")]
    pub interval: u64,
}

pub(crate) fn anchor(layout: &Layout, args: &AnchorArgs) -> Result<i32> {
    let store = open_store(layout)?;
    let s = store.resolve(&args.session)?;
    let interrupted = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(sig, interrupted.clone())?;
    }
    let follow = Follow { layout, follow: true, interrupted: &interrupted };
    let interval = Duration::from_secs(args.interval.max(1));
    let mut out = std::io::stdout().lock();
    let genesis = chain::genesis(s.anchor_token_digest.as_deref());
    let mut head = genesis;
    let mut last_seq: Option<i64> = None;
    let mut emitted: Option<i64> = None;
    let mut last_emit = Instant::now();
    let emit = |out: &mut dyn Write, seq: i64, head: &Hash| -> Result<()> {
        let line = json!({
            "session_id": s.id,
            "seq": seq,
            "chain": chain::hex(head),
            "genesis": chain::hex(&genesis),
            "timestamp": format_rfc3339(now_unix_ns()),
        });
        writeln!(out, "{line}")?;
        out.flush()?;
        Ok(())
    };
    follow.each(&store, &s.id, None, |e| {
        head = chain::link(&head, &chain::leaf(&chain::event_line(&e)));
        last_seq = Some(e.seq);
        if last_emit.elapsed() >= interval {
            emit(&mut out, e.seq, &head)?;
            emitted = Some(e.seq);
            last_emit = Instant::now();
        }
        Ok(())
    })?;
    if let Some(seq) = last_seq.filter(|q| emitted != Some(*q)) {
        emit(&mut out, seq, &head)?;
    }
    Ok(if interrupted.load(Ordering::Relaxed) { 130 } else { 0 })
}
