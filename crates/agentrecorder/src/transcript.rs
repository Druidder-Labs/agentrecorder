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
    pub interactions: Vec<Interaction>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Interaction {
    #[serde(skip)]
    pub at_ns: Option<i64>,
    pub origin: String,
    #[serde(skip)]
    pub text: String,
    pub chars: u64,
    pub images: u64,
    pub api_calls: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

impl Interaction {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_creation
    }
}

const NOT_TYPED: [&str; 3] = ["<local-command-stdout>", "<local-command-stderr>", "<local-command-caveat>"];

fn tag_value<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = s.find(&open)? + open.len();
    let end = s[start..].find(&format!("</{tag}>"))?;
    Some(s[start..start + end].trim())
}

fn prompt(v: &Value) -> Option<(String, String, u64)> {
    if v["type"] != "user" || v["isMeta"] == true || v["isCompactSummary"] == true || v["isSidechain"] == true {
        return None;
    }
    let (mut text, mut images) = (String::new(), 0);
    match &v["message"]["content"] {
        Value::String(s) => text = s.clone(),
        Value::Array(blocks) => {
            for b in blocks {
                match b["type"].as_str() {
                    Some("tool_result") => return None,
                    Some("text") => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(b["text"].as_str().unwrap_or(""));
                    }
                    Some("image") => images += 1,
                    _ => {}
                }
            }
        }
        _ => return None,
    }
    let trimmed = text.trim_start();
    if NOT_TYPED.iter().any(|t| trimmed.starts_with(t)) {
        return None;
    }
    let command = tag_value(trimmed, "command-name").map(|name| match tag_value(trimmed, "command-args") {
        Some(args) if !args.is_empty() => format!("{name} {args}"),
        _ => name.to_string(),
    });
    let origin = match (v["origin"]["kind"].as_str(), &command) {
        (Some(kind), _) => kind.to_string(),
        (None, Some(_)) => "command".into(),
        (None, None) if trimmed.starts_with("<task-notification>") => "task-notification".into(),
        (None, None) => "user".into(),
    };
    Some((origin, command.unwrap_or(text), images))
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_creation
    }
}

pub fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let words: Vec<&str> = flat.split_whitespace().collect();
    let joined = words.join(" ");
    match joined.char_indices().nth(max) {
        Some((cut, _)) => format!("{}...", &joined[..cut]),
        None => joined,
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
        let ts = v["timestamp"].as_str().and_then(parse_rfc3339);
        let outside = ts.is_some_and(|t| t < from_ns || t > to_ns);
        if let Some((origin, text, images)) = prompt(&v) {
            if !outside {
                usage.interactions.push(Interaction {
                    at_ns: ts,
                    origin,
                    chars: text.chars().count() as u64,
                    text,
                    images,
                    ..Interaction::default()
                });
            }
            continue;
        }
        if v["type"] != "assistant" {
            continue;
        }
        let Some(u) = v["message"]["usage"].as_object() else { continue };
        if outside {
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
        if usage.interactions.is_empty() {
            usage.interactions.push(Interaction { at_ns: ts, origin: "none".into(), ..Interaction::default() });
        }
        let turn = usage.interactions.last_mut().expect("pushed above");
        turn.api_calls += 1;
        turn.input += n("input_tokens");
        turn.output += n("output_tokens");
        turn.cache_read += n("cache_read_input_tokens");
        turn.cache_creation += n("cache_creation_input_tokens");
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
            br#"{"type":"user","timestamp":"2026-10-21T10:01:00Z","message":{"role":"user","content":"hi"}}"#.to_vec(),
            line("2026-10-21T10:30:00Z", "req_2", 3, 10, 600, 0),
            line("2026-10-21T09:00:00Z", "req_0", 9, 9, 9, 9),
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
                interactions: vec![
                    Interaction {
                        at_ns: parse_rfc3339("2026-10-21T10:00:01Z"),
                        origin: "none".into(),
                        api_calls: 1,
                        input: 2,
                        output: 100,
                        cache_read: 500,
                        cache_creation: 50,
                        ..Interaction::default()
                    },
                    Interaction {
                        at_ns: parse_rfc3339("2026-10-21T10:01:00Z"),
                        origin: "user".into(),
                        text: "hi".into(),
                        chars: 2,
                        api_calls: 1,
                        input: 3,
                        output: 10,
                        cache_read: 600,
                        ..Interaction::default()
                    },
                ],
            }
        );
        assert_eq!(u.total(), 1265);
        assert_eq!(u.interactions.iter().map(Interaction::total).sum::<u64>(), u.total());
    }

    fn user(ts: &str, extra: serde_json::Value) -> Vec<u8> {
        let mut v = serde_json::json!({"type": "user", "timestamp": ts});
        v.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        v.to_string().into_bytes()
    }

    #[test]
    fn interactions_start_at_typed_prompts_and_ignore_tool_results_and_local_output() {
        let t = "2026-10-21T10:00:00Z";
        let lines = vec![
            user(t, serde_json::json!({"origin": {"kind": "human"}, "message": {"content": "fix the bug"}})),
            line(t, "r1", 1, 10, 0, 0),
            user(t, serde_json::json!({"message": {"content": [{"type": "tool_result", "content": "ok"}]}})),
            line(t, "r2", 1, 20, 0, 0),
            user(t, serde_json::json!({"isMeta": true, "message": {"content": "<local-command-caveat>x"}})),
            user(
                t,
                serde_json::json!({"message": {"content": "<command-name>/model</command-name>\n<command-args>opus</command-args>"}}),
            ),
            user(
                t,
                serde_json::json!({"message": {"content": "<local-command-stdout>Set model</local-command-stdout>"}}),
            ),
            user(t, serde_json::json!({"isCompactSummary": true, "message": {"content": "summary"}})),
            user(
                t,
                serde_json::json!({"origin": {"kind": "human"}, "message": {"content": [
                    {"type": "image", "source": {}}, {"type": "text", "text": "what is this"}
                ]}}),
            ),
            line(t, "r3", 1, 5, 0, 0),
            user(
                t,
                serde_json::json!({"origin": {"kind": "task-notification"}, "message": {"content": "<task-notification>done"}}),
            ),
            user(
                "2026-10-21T09:00:00Z",
                serde_json::json!({"origin": {"kind": "human"}, "message": {"content": "old"}}),
            ),
        ];
        let from = parse_rfc3339("2026-10-21T09:30:00Z").unwrap();
        let u = fold(lines.into_iter(), from, i64::MAX).unwrap();
        let got: Vec<(&str, &str, u64, u64, u64)> = u
            .interactions
            .iter()
            .map(|i| (i.origin.as_str(), i.text.as_str(), i.images, i.api_calls, i.output))
            .collect();
        assert_eq!(
            got,
            [
                ("human", "fix the bug", 0, 2, 30),
                ("command", "/model opus", 0, 0, 0),
                ("human", "what is this", 1, 1, 5),
                ("task-notification", "<task-notification>done", 0, 0, 0),
            ]
        );
    }

    #[test]
    fn one_line_strips_control_characters_and_caps_length() {
        assert_eq!(one_line("a\n\tb\x1b[31m  c", 100), "a b [31m c");
        assert_eq!(one_line("abcdef", 3), "abc...");
        assert_eq!(one_line("cafe\u{301} ok", 5), "cafe\u{301}...");
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
