use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use witnessed_store::{AgentHook, Body, Store, kind, parse_rfc3339};

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
    pub interrupts: Vec<i64>,
    pub refusals: Vec<i64>,
    pub compactions: Vec<i64>,
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
    pub reply_chars: u64,
    pub model: Option<String>,
}

impl Interaction {
    pub const fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_creation
    }
}

/// One interaction as `inspect --json` and the bundle manifest print it; the callers add their own time fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InteractionJson {
    pub index: usize,
    pub origin: String,
    pub chars: u64,
    pub images: u64,
    pub api_calls: u64,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
    pub total: u64,
    pub reply_chars: u64,
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl InteractionJson {
    pub fn new(index: usize, t: &Interaction, with_text: bool) -> Self {
        Self {
            index,
            origin: t.origin.clone(),
            chars: t.chars,
            images: t.images,
            api_calls: t.api_calls,
            input: t.input,
            output: t.output,
            cache_read: t.cache_read,
            cache_creation: t.cache_creation,
            total: t.total(),
            reply_chars: t.reply_chars,
            model: t.model.clone(),
            text: with_text.then(|| t.text.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub agent: String,
    pub path: String,
}

#[derive(Debug)]
pub enum Unavailable {
    NotAgent,
    NoTranscriptPath,
    Unreadable { path: String, cause: String },
}

impl Unavailable {
    pub fn reason(&self) -> String {
        match self {
            Self::NotAgent => crate::adapters::NOT_AGENT.into(),
            Self::NoTranscriptPath => crate::adapters::NO_TRANSCRIPT_PATH.into(),
            Self::Unreadable { path, cause } => crate::adapters::transcript_unreadable(path, cause),
        }
    }
}

impl Located {
    pub fn usage(&self, from_ns: i64, to_ns: i64) -> std::result::Result<Usage, Unavailable> {
        usage(Path::new(&self.path), from_ns, to_ns)
            .map_err(|e| Unavailable::Unreadable { path: self.path.clone(), cause: e.root_cause().to_string() })
    }
}

/// The transcript the agent reported last; `NotAgent` when the session recorded no hooks at all.
pub fn locate_in<'a>(hooks: impl IntoIterator<Item = &'a AgentHook>) -> std::result::Result<Located, Unavailable> {
    let mut any = false;
    let mut found = None;
    for h in hooks {
        any = true;
        if let Some(path) = &h.transcript_path {
            found = Some(Located { agent: h.agent.clone(), path: path.clone() });
        }
    }
    found.ok_or(if any { Unavailable::NoTranscriptPath } else { Unavailable::NotAgent })
}

pub fn locate(store: &Store, session_id: &str) -> Result<std::result::Result<Located, Unavailable>> {
    let mut hooks = Vec::new();
    store.for_each_event(session_id, Some(&[kind::AGENT_HOOK]), |e| {
        if let Some(Body::AgentHook(h)) = e.body() {
            hooks.push(*h);
        }
        Ok(())
    })?;
    Ok(locate_in(&hooks))
}

const NOT_TYPED: [&str; 3] = ["<local-command-stdout>", "<local-command-stderr>", "<local-command-caveat>"];
const INTERRUPTED: &str = "[Request interrupted by user";
const REFUSED: &str = "[Request interrupted by user for tool use";

fn user_text_starts(v: &Value, prefix: &str) -> bool {
    match &v["message"]["content"] {
        Value::String(s) => s.trim_start().starts_with(prefix),
        Value::Array(blocks) => blocks
            .iter()
            .any(|b| b["type"] == "text" && b["text"].as_str().is_some_and(|t| t.trim_start().starts_with(prefix))),
        _ => false,
    }
}

fn reply_chars(v: &Value) -> u64 {
    match &v["message"]["content"] {
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .map(|t| t.chars().count() as u64)
            .sum(),
        Value::String(s) => s.chars().count() as u64,
        _ => 0,
    }
}

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
    if trimmed.starts_with(INTERRUPTED) || NOT_TYPED.iter().any(|t| trimmed.starts_with(t)) {
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
    pub const fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_creation
    }

    fn current_turn(&mut self, at_ns: Option<i64>) -> &mut Interaction {
        if self.interactions.is_empty() {
            self.interactions.push(Interaction { at_ns, origin: "none".into(), ..Interaction::default() });
        }
        let last = self.interactions.len() - 1;
        &mut self.interactions[last]
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
    fold(BufReader::new(file).split(b'\n').map_while(std::result::Result::ok), from_ns, to_ns)
}

pub fn fold(lines: impl Iterator<Item = Vec<u8>>, from_ns: i64, to_ns: i64) -> Result<Usage> {
    let mut usage = Usage::default();
    let mut seen = BTreeSet::new();
    let mut models = BTreeSet::new();
    let mut seen_lines = BTreeSet::new();
    for line in lines {
        let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
        let ts = v["timestamp"].as_str().and_then(parse_rfc3339);
        let outside = ts.is_some_and(|t| t < from_ns || t > to_ns);
        if v["type"] == "user" && !outside {
            if v["isCompactSummary"] == true {
                usage.compactions.extend(ts);
                continue;
            }
            if user_text_starts(&v, REFUSED) {
                usage.refusals.extend(ts);
                continue;
            }
            if user_text_starts(&v, INTERRUPTED) {
                usage.interrupts.extend(ts);
                continue;
            }
        }
        if v["type"] == "assistant"
            && !outside
            && v["message"]["model"] != "<synthetic>"
            && v["uuid"].as_str().is_none_or(|u| seen_lines.insert(u.to_string()))
        {
            let turn = usage.current_turn(ts);
            turn.reply_chars += reply_chars(&v);
            if let Some(m) = v["message"]["model"].as_str() {
                turn.model = Some(m.to_string());
            }
        }
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
        let turn = usage.current_turn(ts);
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
                        model: Some("claude-fable-5-1".into()),
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
                        model: Some("claude-fable-5-1".into()),
                        ..Interaction::default()
                    },
                ],
                ..Usage::default()
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
            user(t, serde_json::json!({"message": {"content": [{"type": "text", "text": "[Request interrupted by user]"}]}})),
            user(t, serde_json::json!({"message": {"content": [{"type": "text", "text": "[Request interrupted by user for tool use]"}]}})),
            br#"{"type":"assistant","timestamp":"2026-10-21T10:00:00Z","uuid":"a1","message":{"content":[{"type":"text","text":"done, 12 chars"},{"type":"tool_use","name":"Bash"}]}}"#.to_vec(),
            br#"{"type":"assistant","timestamp":"2026-10-21T10:00:00Z","uuid":"a1","message":{"content":[{"type":"text","text":"done, 12 chars"}]}}"#.to_vec(),
            br#"{"type":"assistant","timestamp":"2026-10-21T10:00:00Z","uuid":"s1","message":{"model":"<synthetic>","content":[{"type":"text","text":"No response requested."}]}}"#.to_vec(),
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
        let t_ns = parse_rfc3339(t).unwrap();
        assert_eq!(
            (u.compactions.clone(), u.interrupts.clone(), u.refusals.clone()),
            (vec![t_ns], vec![t_ns], vec![t_ns])
        );
        assert_eq!(u.interactions.iter().map(|i| i.reply_chars).collect::<Vec<_>>(), [0, 14, 0, 0]);
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
    fn locates_the_last_reported_transcript_or_says_why_not() {
        assert!(matches!(locate_in([]), Err(Unavailable::NotAgent)));
        let h = |path: Option<&str>| AgentHook {
            agent: "claude".into(),
            hook_event_name: "Stop".into(),
            transcript_path: path.map(str::to_string),
            ..AgentHook::default()
        };
        assert!(matches!(locate_in(&[h(None)]), Err(Unavailable::NoTranscriptPath)));
        let found = locate_in(&[h(Some("/a")), h(Some("/b")), h(None)]).unwrap();
        assert_eq!(found, Located { agent: "claude".into(), path: "/b".into() });
        let err = found.usage(0, i64::MAX).unwrap_err();
        assert!(matches!(&err, Unavailable::Unreadable { path, .. } if path == "/b"));
        assert!(err.reason().starts_with("the agent transcript cannot be read: /b ("), "{}", err.reason());
    }

    #[test]
    fn a_missing_file_is_an_error_naming_the_path() {
        let err = usage(Path::new("/nonexistent/t.jsonl"), 0, i64::MAX).unwrap_err();
        assert!(format!("{err:#}").contains("/nonexistent/t.jsonl"));
    }
}
