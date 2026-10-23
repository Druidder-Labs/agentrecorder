use anyhow::{Result, bail};
use serde::Serialize;
use witnessed_store::kind;

use super::assemble::{Fate, Item};

const CUTTABLE: &[&str] = &[
    kind::TERMINAL_OUTPUT,
    kind::TERMINAL_INPUT,
    kind::TERMINAL_SNAPSHOT,
    kind::TERMINAL_NOTIFICATION,
    kind::AGENT_HOOK,
    kind::SHELL_COMMAND_FINISHED,
    kind::USER_MARK,
];

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) struct CutRange {
    pub from_ns: i64,
    pub to_ns: i64,
}

pub(crate) fn parse_cut(s: &str) -> Result<CutRange> {
    let Some((from, to)) = s.split_once('-') else {
        bail!("cut {s:?} must look like FROM-TO, such as 00:12:03-00:12:09")
    };
    let (from_ns, to_ns) = (crate::fmt::parse_offset(from)?, crate::fmt::parse_offset(to)?);
    if to_ns <= from_ns {
        bail!("cut {s:?} ends before it starts");
    }
    Ok(CutRange { from_ns, to_ns })
}

/// One applied `--cut`, in milliseconds as the manifest records it.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize)]
pub(crate) struct CutReport {
    pub events: u64,
    pub from_ms: i64,
    pub to_ms: i64,
}

impl CutReport {
    pub(crate) const fn contains(&self, t_ms: i64) -> bool {
        t_ms >= self.from_ms && t_ms < self.to_ms
    }

    pub(crate) const fn range(&self) -> CutRange {
        CutRange { from_ns: self.from_ms * 1_000_000, to_ns: self.to_ms * 1_000_000 }
    }

    pub(crate) fn label(&self) -> String {
        format!("cut {}-{}", crate::fmt::offset_ms(self.from_ms), crate::fmt::offset_ms(self.to_ms))
    }
}

pub(crate) fn in_cut(cuts: &[CutReport], t_ms: i64) -> bool {
    cuts.iter().any(|c| c.contains(t_ms))
}

pub(crate) fn apply_cuts(items: &mut [Item], cuts: &[CutRange]) -> Vec<CutReport> {
    cuts.iter()
        .map(|c| {
            let mut events = 0;
            for it in items.iter_mut() {
                let e = &it.event;
                if e.mono_ns >= c.from_ns && e.mono_ns < c.to_ns && CUTTABLE.contains(&e.kind.as_str()) {
                    it.fate = Fate::Cut;
                    events += 1;
                }
            }
            CutReport { events, from_ms: c.from_ns / 1_000_000, to_ms: c.to_ns / 1_000_000 }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::super::assemble::test_item as ev;
    use super::*;

    #[test]
    fn cuts_only_content_events_inside_the_range() {
        let mut items = vec![
            ev(0, kind::TERMINAL_OUTPUT, 900, Some(b"a"), Value::Null),
            ev(1, kind::TERMINAL_OUTPUT, 1000, Some(b"b"), Value::Null),
            ev(2, kind::TERMINAL_RESIZED, 1500, None, json!({"rows": 1, "cols": 1})),
            ev(3, kind::USER_MARK, 1999, None, json!({"label": "x"})),
            ev(4, kind::TERMINAL_OUTPUT, 2000, Some(b"c"), Value::Null),
        ];
        let report = apply_cuts(&mut items, &[parse_cut("1-2").unwrap()]);
        assert_eq!(report, [CutReport { events: 2, from_ms: 1000, to_ms: 2000 }]);
        assert_eq!(serde_json::to_value(&report).unwrap(), json!([{"from_ms": 1000, "to_ms": 2000, "events": 2}]));
        assert!(in_cut(&report, 1000) && in_cut(&report, 1999) && !in_cut(&report, 2000));
        assert_eq!(report[0].range(), CutRange { from_ns: 1_000_000_000, to_ns: 2_000_000_000 });
        assert_eq!(report[0].label(), "cut 00:00:01.0-00:00:02.0");
        let fates: Vec<Fate> = items.iter().map(|i| i.fate).collect();
        assert_eq!(fates, [Fate::Kept, Fate::Cut, Fate::Kept, Fate::Cut, Fate::Kept]);
    }

    #[test]
    fn parses_cut_ranges() {
        assert_eq!(
            parse_cut("00:01:00-00:01:30").unwrap(),
            CutRange { from_ns: 60_000_000_000, to_ns: 90_000_000_000 }
        );
        assert!(parse_cut("5-2").is_err());
        assert!(parse_cut("5").is_err());
    }
}
