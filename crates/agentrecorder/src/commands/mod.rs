pub mod export;
pub mod hook;
pub mod inspect;
pub mod recover;
pub mod replay;
pub mod run;
pub mod sessions;

use agentrecorder_store::Store;
use agentrecorder_supervisor::Layout;
use anyhow::{Result, bail};

pub fn open_store(layout: &Layout) -> Result<Store> {
    let path = layout.db_path();
    if !path.exists() {
        bail!(
            "no sessions recorded yet in {}; start one with `agentrecorder run -- <command>`",
            layout.root().display()
        );
    }
    layout.ensure()?;
    Store::open(&path)
}
