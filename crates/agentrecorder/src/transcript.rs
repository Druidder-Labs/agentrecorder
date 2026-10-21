use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use agentrecorder_store::parse_rfc3339;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub api_calls: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub models: Vec<String>,
    pub outside_session: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_creation
    }
}

pub fn usage(path: &Path, from_ns: i64, to_ns: i64) -> Result<Usage> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    fold(BufReader::new(file).split(b'\n').map_while(|l| l.ok()), from_ns, to_ns)
}

pub fn fold(lines: impl Iterator<Item = Vec<u8>>, from_ns: i64, to_ns: i64) -> Result<Usage> {
    let mut usage = Usage::default();
    let mut seen = BTreeSet::new();
    let mut models = BTreeSet::new();
    for line in lines {
        let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
        if v["type"] != "assistant" {
            continue;
        }
        let Some(u) = v["message"]["usage"].as_object() else { continue };
        if let Some(ts) = v["timestamp"].as_str().and_then(parse_rfc3339)
            && (ts < from_ns || ts > to_ns)
        {
            usage.outside_session += 1;
            continue;
        }
        let key = ["requestId", "uuid"]
            .iter()
            .find_map(|k| v[k].as_str())
            .or_else(|| v["message"]["id"].as_str())
            .map(str::to_string);
        if let Some(k) = key
            && !seen.insert(k)
        {
            continue;
        }
        let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        usage.api_calls += 1;
        usage.input += n("input_tokens");
        usage.output += n("output_tokens");
        usage.cache_read += n("cache_read_input_tokens");
        usage.cache_creation += n("cache_creation_input_tokens");
        if let Some(m) = v["message"]["model"].as_str() {
            models.insert(m.to_string());
        }
    }
    usage.models = models.into_iter().collect();
    Ok(usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(ts: &str, req: &str, input: u64, output: u64, read: u64, create: u64) -> Vec<u8> {
        serde_json::json!({
            "type": "assistant",
            "timestamp": ts,
            "requestId": req,
            "message": {
                "model": "claude-fable-5-1",
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_input_tokens": read,
                    "cache_creation_input_tokens": create,
                },
            },
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn sums_one_usage_per_request_inside_the_session_window() {
        let from = parse_rfc3339("2026-10-21T10:00:00Z").unwrap();
        let to = parse_rfc3339("2026-10-21T11:00:00Z").unwrap();
        let lines = vec![
            line("2026-10-21T10:00:01Z", "req_1", 2, 100, 500, 50),
            line("2026-10-21T10:00:01Z", "req_1", 2, 100, 500, 50),
            line("2026-10-21T10:30:00Z", "req_2", 3, 10, 600, 0),
            line("2026-10-21T09:00:00Z", "req_0", 9, 9, 9, 9),
            br#"{"type":"user","timestamp":"2026-10-21T10:01:00Z","message":{"role":"user","content":"hi"}}"#.to_vec(),
            b"not json".to_vec(),
        ];
        let u = fold(lines.into_iter(), from, to).unwrap();
        assert_eq!(
            u,
            Usage {
                api_calls: 2,
                input: 5,
                output: 110,
                cache_read: 1100,
                cache_creation: 50,
                models: vec!["claude-fable-5-1".into()],
                outside_session: 1,
            }
        );
        assert_eq!(u.total(), 1265);
    }

    #[test]
    fn lines_without_a_timestamp_or_request_id_are_counted_once_each() {
        let a =
            br#"{"type":"assistant","uuid":"u1","message":{"usage":{"input_tokens":1,"output_tokens":1}}}"#.to_vec();
        let b = br#"{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":1,"output_tokens":1}}}"#.to_vec();
        let u = fold(vec![a.clone(), a, b.clone(), b].into_iter(), 0, i64::MAX).unwrap();
        assert_eq!((u.api_calls, u.input, u.output), (2, 2, 2));
        assert!(u.models.is_empty());
    }

    #[test]
    fn a_missing_file_is_an_error_naming_the_path() {
        let err = usage(Path::new("/nonexistent/t.jsonl"), 0, i64::MAX).unwrap_err();
        assert!(format!("{err:#}").contains("/nonexistent/t.jsonl"));
    }
}
