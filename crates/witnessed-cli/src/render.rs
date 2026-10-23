use anyhow::Result;
use witnessed_store::{Body, Event, Session, Store, kind};
use witnessed_supervisor::screen::ScreenModel;

pub const SCREEN_KINDS: &[&str] = &[kind::TERMINAL_OUTPUT, kind::TERMINAL_RESIZED, kind::TERMINAL_SNAPSHOT];

pub fn initial_size(store: &Store, s: &Session) -> Result<(u16, u16)> {
    let mut size = None;
    store.for_each_event(&s.id, Some(&[kind::SESSION_CREATED]), |e| {
        if size.is_none()
            && let Some(Body::SessionCreated { rows, cols, .. }) = e.body()
        {
            size = Some((rows, cols));
        }
        Ok(())
    })?;
    Ok(size.unwrap_or((s.rows.clamp(1, u16::MAX as i64) as u16, s.cols.clamp(1, u16::MAX as i64) as u16)))
}

pub fn recorded_screen((rows, cols): (u16, u16)) -> ScreenModel {
    ScreenModel::new(rows, cols, 0)
}

pub fn apply(model: &mut ScreenModel, e: &Event) -> bool {
    match e.body() {
        Some(Body::TerminalOutput) => {
            if let Some(d) = &e.data {
                model.process(d);
            }
            true
        }
        Some(Body::TerminalResized { rows, cols }) => {
            model.resize(rows, cols);
            false
        }
        Some(Body::TerminalSnapshot { rows, cols, .. }) => {
            *model = recorded_screen((rows, cols));
            if let Some(d) = &e.data {
                model.process(d);
            }
            true
        }
        _ => false,
    }
}

pub fn playback_start(store: &Store, s: &Session, at_ns: i64) -> Result<(ScreenModel, i64)> {
    let mut model = recorded_screen(initial_size(store, s)?);
    let Some(seq) = store.last_seq_at_or_before(&s.id, kind::TERMINAL_SNAPSHOT, at_ns)? else {
        return Ok((model, -1));
    };
    let mut found = false;
    store.for_each_event_from(&s.id, seq - 1, Some(&[kind::TERMINAL_SNAPSHOT]), |e| {
        if !found {
            found = apply(&mut model, &e);
        }
        Ok(())
    })?;
    Ok((model, if found { seq } else { -1 }))
}

pub fn final_screen(store: &Store, s: &Session) -> Result<Option<String>> {
    let (mut model, after) = playback_start(store, s, i64::MAX)?;
    let mut any = after >= 0;
    store.for_each_event_from(&s.id, after, Some(SCREEN_KINDS), |e| {
        any |= apply(&mut model, &e);
        Ok(())
    })?;
    Ok(any.then(|| model.text()))
}
