pub mod bundle;
pub mod export;
pub mod hook;
pub mod inspect;
pub mod recover;
pub mod replay;
pub mod review;
pub mod run;
pub mod sessions;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use witnessed_store::{Event, Store};
use witnessed_supervisor::Layout;

pub fn open_store(layout: &Layout) -> Result<Store> {
    let path = layout.db_path();
    if !path.exists() {
        bail!("no sessions yet; record one with: witnessed run -- claude");
    }
    layout.ensure()?;
    Store::open(&path)
}

const FOLLOW_POLL: Duration = Duration::from_millis(100);

pub struct Follow<'a> {
    pub layout: &'a Layout,
    pub follow: bool,
    pub interrupted: &'a AtomicBool,
}

impl Follow<'_> {
    pub fn each_from(
        &self,
        store: &Store,
        session_id: &str,
        after_seq: i64,
        kinds: Option<&[&str]>,
        mut f: impl FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        let mut last = after_seq;
        let read = |last: &mut i64, f: &mut dyn FnMut(Event) -> Result<()>| {
            store.for_each_event_from(session_id, *last, kinds, |e| {
                *last = e.seq;
                f(e)
            })
        };
        read(&mut last, &mut f)?;
        while self.follow && !self.interrupted.load(Ordering::Relaxed) {
            let Some(mut s) = store.session(session_id)? else { return Ok(()) };
            if crate::state::resolve(self.layout, store, &mut s).is_finished() {
                return read(&mut last, &mut f);
            }
            let wake = std::time::Instant::now() + FOLLOW_POLL;
            while std::time::Instant::now() < wake && !self.interrupted.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
            }
            read(&mut last, &mut f)?;
        }
        Ok(())
    }

    pub fn each(
        &self,
        store: &Store,
        id: &str,
        kinds: Option<&[&str]>,
        f: impl FnMut(Event) -> Result<()>,
    ) -> Result<()> {
        self.each_from(store, id, -1, kinds, f)
    }
}
