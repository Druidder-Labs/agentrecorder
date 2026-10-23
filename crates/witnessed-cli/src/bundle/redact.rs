use anyhow::{Context, Result, bail};
use regex::bytes::Regex;
use serde_json::{Value, json};
use witnessed_store::{Event, kind};

use super::assemble::{Fate, Item};

const SECRET_PATTERNS: &[(&str, &str)] = &[
    ("openai or anthropic key", r"sk-[A-Za-z0-9_-]{16,}"),
    ("github token", r"(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36}|github_pat_[A-Za-z0-9_]{22,}"),
    ("gitlab token", r"glpat-[A-Za-z0-9_-]{20,}"),
    ("aws access key id", r"(?:AKIA|ASIA)[0-9A-Z]{16}"),
    ("google api key", r"AIza[0-9A-Za-z_-]{35}"),
    ("slack token", r"xox[abprs]-[A-Za-z0-9-]{10,}"),
    ("bearer token", r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{20,}"),
    ("jwt", r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}"),
    ("private key block", r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----"),
];

const BYTE_STREAMS: &[&str] = &[kind::TERMINAL_OUTPUT, kind::TERMINAL_INPUT];

pub(crate) fn patterns(redact: &[String], literal: &[String], secrets: bool) -> Result<Vec<Regex>> {
    let mut out = Vec::new();
    for p in redact {
        out.push(Regex::new(p).with_context(|| format!("invalid --redact pattern {p:?}"))?);
    }
    for t in literal {
        if t.is_empty() {
            bail!("--redact-literal needs a non-empty text");
        }
        out.push(Regex::new(&regex::escape(t)).expect("escaped literal"));
    }
    if secrets {
        for (_, p) in SECRET_PATTERNS {
            out.push(Regex::new(p).expect("built-in pattern"));
        }
    }
    Ok(out)
}

pub(crate) struct Match {
    pub mono_ns: i64,
    pub kind: String,
    pub preview: String,
}

impl Match {
    pub(crate) fn to_json(&self) -> Value {
        json!({"offset_ms": self.mono_ns / 1_000_000, "type": self.kind, "preview": self.preview})
    }
}

pub(crate) fn preview(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let shown: String = text.chars().take(4).collect();
    format!("{shown}{} ({} bytes)", "*".repeat(text.chars().count().saturating_sub(4).min(12)), bytes.len())
}

pub(crate) fn mask(buf: &mut [u8], patterns: &[Regex]) -> Vec<(usize, String)> {
    let mut ranges = Vec::new();
    for p in patterns {
        ranges.extend(p.find_iter(buf).filter(|m| !m.is_empty()).map(|m| m.range()));
    }
    ranges.sort_by_key(|r| r.start);
    let found = ranges.iter().map(|r| (r.start, preview(&buf[r.clone()]))).collect();
    for r in &ranges {
        buf[r.clone()].fill(b'*');
    }
    found
}

pub(crate) fn mask_string(s: &mut String, patterns: &[Regex]) -> Vec<String> {
    let mut bytes = s.as_bytes().to_vec();
    let found = mask(&mut bytes, patterns);
    if !found.is_empty() {
        *s = String::from_utf8_lossy(&bytes).into_owned();
    }
    found.into_iter().map(|(_, p)| p).collect()
}

pub(crate) fn mask_value(v: &mut Value, patterns: &[Regex]) -> Vec<String> {
    match v {
        Value::String(s) => mask_string(s, patterns),
        Value::Array(a) => a.iter_mut().flat_map(|x| mask_value(x, patterns)).collect(),
        Value::Object(o) => o.values_mut().flat_map(|x| mask_value(x, patterns)).collect(),
        _ => Vec::new(),
    }
}

/// Masks output and input as one stream per kind, so a secret split across two chunks is still found.
pub(crate) fn apply_redactions(items: &mut [Item], patterns: &[Regex]) -> Vec<Match> {
    let mut matches = Vec::new();
    if patterns.is_empty() {
        return matches;
    }
    for stream in BYTE_STREAMS {
        let mut targets: Vec<(i64, &str, &mut Vec<u8>, &mut Fate)> = items
            .iter_mut()
            .filter(|it| it.fate != Fate::Cut && it.event.kind == *stream)
            .filter_map(|it| {
                let Item { event, fate, .. } = it;
                let Event { mono_ns, kind, data, .. } = event;
                data.as_mut().map(|d| (*mono_ns, kind.as_str(), d, fate))
            })
            .collect();
        let mut joined = Vec::with_capacity(targets.iter().map(|t| t.2.len()).sum());
        let mut starts = Vec::with_capacity(targets.len());
        for (_, _, data, _) in &targets {
            starts.push(joined.len());
            joined.extend_from_slice(data);
        }
        for (start, preview) in mask(&mut joined, patterns) {
            let n = starts.partition_point(|&s| s <= start).saturating_sub(1);
            let (mono_ns, kind, _, _) = &targets[n];
            matches.push(Match { mono_ns: *mono_ns, kind: (*kind).to_string(), preview });
        }
        let mut at = 0;
        for (_, _, data, fate) in &mut targets {
            let new = &joined[at..at + data.len()];
            at += data.len();
            if new != data.as_slice() {
                data.copy_from_slice(new);
                **fate = Fate::Redacted;
            }
        }
    }
    for it in items.iter_mut().filter(|it| it.fate != Fate::Cut) {
        let mut found = mask_value(&mut it.event.payload, patterns);
        if it.event.kind == kind::TERMINAL_SNAPSHOT
            && let Some(d) = it.event.data.as_mut()
        {
            found.extend(mask(d, patterns).into_iter().map(|(_, p)| p));
        }
        if !found.is_empty() {
            it.fate = Fate::Redacted;
            let e = &it.event;
            matches.extend(found.into_iter().map(|preview| Match {
                mono_ns: e.mono_ns,
                kind: e.kind.clone(),
                preview,
            }));
        }
    }
    matches.sort_by_key(|m| m.mono_ns);
    matches
}

/// Redaction cannot read compressed binary patch data, so with redaction on, each binary patch is replaced by the
/// line git prints for binary files without --binary.
pub(crate) fn drop_binary_patches(diff: &[u8]) -> (Vec<u8>, usize) {
    let text = String::from_utf8_lossy(diff);
    if !text.contains("\nGIT binary patch\n") {
        return (diff.to_vec(), 0);
    }
    let mut out = Vec::with_capacity(diff.len());
    let mut dropped = 0;
    let mut rest = diff;
    while !rest.is_empty() {
        let next = rest[1..].windows(12).position(|w| w == b"\ndiff --git ").map_or(rest.len(), |i| i + 2);
        let (section, tail) = rest.split_at(next);
        if let Some(at) = section.windows(17).position(|w| w == b"GIT binary patch\n") {
            let header = &section[..at];
            let name = String::from_utf8_lossy(header.split(|b| *b == b'\n').next().unwrap_or_default())
                .trim_start_matches("diff --git ")
                .to_string();
            let (a, b) =
                name.split_once(" b/").map_or((name.clone(), name.clone()), |(a, b)| (a.to_string(), format!("b/{b}")));
            out.extend_from_slice(header);
            out.extend_from_slice(format!("Binary files {a} and {b} differ\n").as_bytes());
            dropped += 1;
        } else {
            out.extend_from_slice(section);
        }
        rest = tail;
    }
    (out, dropped)
}

#[cfg(test)]
mod tests {
    use super::super::assemble::test_item as ev;
    use super::*;

    #[test]
    fn redacts_matches_split_across_output_events_and_in_payloads() {
        let mut items = vec![
            ev(0, kind::TERMINAL_OUTPUT, 1, Some(b"token=sk-AB"), Value::Null),
            ev(1, kind::TERMINAL_OUTPUT, 2, Some(b"CD done"), Value::Null),
            ev(2, kind::TERMINAL_NOTIFICATION, 3, None, json!({"source": "osc9", "text": "key sk-XY1"})),
            ev(3, kind::TERMINAL_OUTPUT, 4, Some(b"clean"), Value::Null),
        ];
        let found = apply_redactions(&mut items, &[Regex::new("sk-[A-Z0-9]+").unwrap()]);
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].mono_ns, found[0].preview.as_str()), (1_000_000, "sk-A*** (7 bytes)"));
        assert_eq!(found[1].kind, kind::TERMINAL_NOTIFICATION);
        assert_eq!(items[0].event.data.as_deref(), Some(&b"token=*****"[..]));
        assert_eq!(items[1].event.data.as_deref(), Some(&b"** done"[..]));
        assert_eq!(items[2].event.payload["text"], "key ******");
        let fates: Vec<Fate> = items.iter().map(|i| i.fate).collect();
        assert_eq!(fates, [Fate::Redacted, Fate::Redacted, Fate::Redacted, Fate::Kept]);
    }

    #[test]
    fn built_in_secret_patterns_compile_and_match_their_shapes() {
        let ps = patterns(&[], &["a(b".into()], true).unwrap();
        assert_eq!(ps.len(), SECRET_PATTERNS.len() + 1);
        assert!(ps[0].is_match(b"xa(bx") && !ps[0].is_match(b"ab"));
        let samples: &[&[u8]] = &[
            b"sk-ant-api03-abcdefghijklmnop",
            b"ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            b"AKIAABCDEFGHIJKLMNOP",
            b"Bearer abcdefghijklmnopqrstuvwxyz",
            b"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.abcdefghijklmnop",
            b"-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----",
        ];
        for s in samples {
            assert!(ps[1..].iter().any(|p| p.is_match(s)), "{}", String::from_utf8_lossy(s));
        }
        assert!(!ps[1..].iter().any(|p| p.is_match(b"cargo build --release")));
        assert!(patterns(&[], &[String::new()], false).is_err());
        assert!(patterns(&["(".into()], &[], false).is_err());
    }

    #[test]
    fn binary_patches_are_reduced_when_redacting() {
        let diff = b"diff --git a/t.txt b/t.txt\n--- a/t.txt\n+++ b/t.txt\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/r.zip b/r.zip\nnew file mode 100644\nindex 0000000..1111111\nGIT binary patch\nliteral 3\nKcmZ?l0000\n\nliteral 0\nHcmV?d00001\n\ndiff --git a/u.txt b/u.txt\n--- a/u.txt\n+++ b/u.txt\n@@ -1 +1 @@\n-c\n+d\n";
        let (out, dropped) = drop_binary_patches(diff);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(dropped, 1);
        assert!(!text.contains("GIT binary patch") && !text.contains("KcmZ"), "{text}");
        assert!(
            text.contains("new file mode 100644\nindex 0000000..1111111\nBinary files a/r.zip and b/r.zip differ\n"),
            "{text}"
        );
        assert!(text.contains("+b\n") && text.ends_with("+d\n"), "{text}");
        let plain = b"diff --git a/t.txt b/t.txt\n@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(drop_binary_patches(plain), (plain.to_vec(), 0));
    }

    #[test]
    fn mask_string_keeps_a_clean_string_untouched() {
        let ps = patterns(&["sk-[0-9]+".into()], &[], false).unwrap();
        let mut s = "clean".to_string();
        assert!(mask_string(&mut s, &ps).is_empty());
        let mut s = "id sk-12345 end".to_string();
        assert_eq!(mask_string(&mut s, &ps), ["sk-1**** (8 bytes)"]);
        assert_eq!(s, "id ******** end");
    }
}
