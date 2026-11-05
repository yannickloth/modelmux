//! Translation between the OpenAI chat-completions format (what agents
//! speak) and the Anthropic /v1/messages format (what some providers speak).
//!
//! Request: OpenAI -> Anthropic (messages, system, tools, tool_choice,
//! stop, max_tokens, temperature, top_p, stream).
//! Response: Anthropic -> OpenAI, both JSON and SSE.

use serde_json::{json, Value};

use crate::sse::SseEvent;

/// Translate an OpenAI chat-completions request body into an Anthropic
/// /v1/messages request body.
pub fn translate_request(body: &Value, default_max_tokens: u64) -> Result<Value, String> {
    let msgs = body
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "missing 'messages'".to_string())?;

    let mut system_parts: Vec<String> = vec![];
    let mut out_msgs: Vec<Value> = vec![];

    for m in msgs {
        let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let content = m.get("content");
        match role {
            "system" | "developer" => {
                if let Some(t) = content_to_text(content) {
                    system_parts.push(t);
                }
            }
            "user" => {
                out_msgs.push(json!({ "role": "user", "content": user_blocks(content) }));
            }
            "assistant" => {
                let mut blocks: Vec<Value> = vec![];
                if let Some(t) = content_to_text(content) {
                    if !t.is_empty() {
                        blocks.push(json!({ "type": "text", "text": t }));
                    }
                }
                if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("call_0");
                        let name = tc
                            .pointer("/function/name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let raw_args = tc
                            .pointer("/function/arguments")
                            .cloned()
                            .unwrap_or(Value::Null);
                        let input = match &raw_args {
                            Value::String(s) => {
                                serde_json::from_str(s).unwrap_or_else(|_| json!({}))
                            }
                            Value::Null => json!({}),
                            other => other.clone(),
                        };
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }));
                    }
                }
                if blocks.is_empty() {
                    blocks.push(json!({ "type": "text", "text": "" }));
                }
                out_msgs.push(json!({ "role": "assistant", "content": blocks }));
            }
            "tool" => {
                let tool_call_id = m.get("tool_call_id").and_then(|v| v.as_str()).unwrap_or("");
                let text = content_to_text(content).unwrap_or_default();
                out_msgs.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": [{ "type": "text", "text": text }],
                    }],
                }));
            }
            _ => {}
        }
    }

    if out_msgs.is_empty() {
        return Err("no usable messages after translation".to_string());
    }

    let max_tokens = body
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(default_max_tokens)
        .max(1);

    let mut out = json!({
        "model": body
            .get("model")
            .cloned()
            .unwrap_or_else(|| Value::String("claude-sonnet-4-20250514".into())),
        "messages": out_msgs,
        "max_tokens": max_tokens,
    });

    if !system_parts.is_empty() {
        out["system"] = Value::String(system_parts.join("\n\n"));
    }
    if let Some(t) = body.get("temperature") {
        out["temperature"] = t.clone();
    }
    if let Some(t) = body.get("top_p") {
        out["top_p"] = t.clone();
    }
    if let Some(stop) = body.get("stop") {
        let seq: Vec<Value> = match stop {
            Value::String(s) => vec![Value::String(s.clone())],
            Value::Array(a) => a.clone(),
            _ => vec![],
        };
        if !seq.is_empty() {
            out["stop_sequences"] = Value::Array(seq);
        }
    }
    if body.get("stream").and_then(|v| v.as_bool()) == Some(true) {
        out["stream"] = Value::Bool(true);
    }
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let mut t_out: Vec<Value> = vec![];
        for t in tools {
            if t.get("type").and_then(|v| v.as_str()) == Some("function") {
                if let Some(f) = t.get("function") {
                    t_out.push(json!({
                        "name": f.get("name").cloned().unwrap_or(Value::String(String::new())),
                        "description": f.get("description").cloned().unwrap_or(Value::String(String::new())),
                        "input_schema": f.get("parameters").cloned().unwrap_or(json!({ "type": "object" })),
                    }));
                }
            } else {
                t_out.push(t.clone());
            }
        }
        if !t_out.is_empty() {
            out["tools"] = Value::Array(t_out);
        }
    }
    if let Some(tc) = body.get("tool_choice") {
        let mapped = match tc {
            Value::String(s) => match s.as_str() {
                "none" => json!({ "type": "none" }),
                "required" => json!({ "type": "any" }),
                _ => json!({ "type": "auto" }),
            },
            Value::Object(o) => {
                if o.get("type").and_then(|v| v.as_str()) == Some("function") {
                    let name = o
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .cloned()
                        .unwrap_or(Value::String(String::new()));
                    json!({ "type": "tool", "name": name })
                } else {
                    tc.clone()
                }
            }
            _ => json!({ "type": "auto" }),
        };
        out["tool_choice"] = mapped;
    }
    Ok(out)
}

/// Translate an Anthropic /v1/messages JSON response into an OpenAI
/// chat.completion body.
pub fn translate_response(v: &Value, alias: &str) -> Value {
    let id = v.get("id").and_then(|x| x.as_str()).unwrap_or("msg");
    let mut text_parts: Vec<String> = vec![];
    let mut tool_calls: Vec<Value> = vec![];
    if let Some(blocks) = v.get("content").and_then(|x| x.as_array()) {
        for b in blocks {
            match b.get("type").and_then(|x| x.as_str()) {
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(|x| x.as_str()) {
                        text_parts.push(t.to_string());
                    }
                }
                Some("tool_use") => {
                    let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                    let args = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                    let tid = b.get("id").cloned().unwrap_or_else(|| json!("toolu_0"));
                    let name = b
                        .get("name")
                        .cloned()
                        .unwrap_or(Value::String(String::new()));
                    tool_calls.push(json!({
                        "index": tool_calls.len(),
                        "id": tid,
                        "type": "function",
                        "function": { "name": name, "arguments": args },
                    }));
                }
                _ => {}
            }
        }
    }
    let finish = finish_reason(v.get("stop_reason").and_then(|x| x.as_str()));
    let usage = v.get("usage");
    let prompt_tokens = usage
        .map(|u| {
            u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0)
                + u.get("cache_read_input_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0)
                + u.get("cache_creation_input_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0)
        })
        .unwrap_or(0);
    let completion_tokens = usage
        .and_then(|u| u.get("output_tokens").and_then(|x| x.as_u64()))
        .unwrap_or(0);

    let mut message = json!({ "role": "assistant" });
    let text = text_parts.join("");
    message["content"] = if text.is_empty() && !tool_calls.is_empty() {
        Value::Null
    } else {
        Value::String(text)
    };
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }

    json!({
        "id": format!("chatcmpl-{id}"),
        "object": "chat.completion",
        "created": chrono::Utc::now().timestamp(),
        "model": alias,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish,
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        },
    })
}

pub fn finish_reason(r: Option<&str>) -> &'static str {
    match r {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        Some("refusal") => "content_filter",
        _ => "stop",
    }
}

fn content_to_text(content: Option<&Value>) -> Option<String> {
    let v = content?;
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut out: Vec<String> = vec![];
            for p in parts {
                if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                    out.push(t.to_string());
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out.join("\n"))
            }
        }
        _ => None,
    }
}

fn user_blocks(content: Option<&Value>) -> Vec<Value> {
    let mut blocks: Vec<Value> = vec![];
    match content {
        Some(Value::String(s)) => {
            blocks.push(json!({ "type": "text", "text": s }));
        }
        Some(Value::Array(parts)) => {
            for p in parts {
                match p.get("type").and_then(|x| x.as_str()) {
                    Some("text") => {
                        if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                            blocks.push(json!({ "type": "text", "text": t }));
                        }
                    }
                    Some("image_url") => {
                        let url = p
                            .pointer("/image_url/url")
                            .and_then(|x| x.as_str())
                            .unwrap_or("");
                        if let Some((media_type, b64)) = parse_data_uri(url) {
                            blocks.push(json!({
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": media_type,
                                    "data": b64,
                                },
                            }));
                        } else if !url.is_empty() {
                            blocks.push(json!({
                                "type": "image",
                                "source": { "type": "url", "url": url },
                            }));
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if blocks.is_empty() {
        blocks.push(json!({ "type": "text", "text": "" }));
    }
    blocks
}

fn parse_data_uri(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, b64) = rest.split_once(',')?;
    if !meta.ends_with(";base64") {
        return None;
    }
    let media = meta[..meta.len() - ";base64".len()].to_string();
    Some((media, b64.to_string()))
}

/// Incremental Anthropic-SSE -> OpenAI-chunk-SSE translator.
pub struct StreamTlx {
    pub alias: String,
    pub chat_id: String,
    pub created: i64,
    pub include_usage: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
    cur_tool: Option<usize>,
    tool_seq: usize,
    pub started: bool,
    pub finished: bool,
    finish_reason: Option<String>,
}

impl StreamTlx {
    pub fn new(alias: &str, include_usage: bool) -> Self {
        Self {
            alias: alias.to_string(),
            chat_id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            created: chrono::Utc::now().timestamp(),
            include_usage,
            input_tokens: 0,
            output_tokens: 0,
            cur_tool: None,
            tool_seq: 0,
            started: false,
            finished: false,
            finish_reason: None,
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> String {
        let obj = json!({
            "id": self.chat_id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.alias,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish,
            }],
        });
        format!("data: {}\n\n", obj)
    }

    fn ensure_start(&mut self, out: &mut Vec<String>) {
        if !self.started {
            self.started = true;
            out.push(self.chunk(json!({ "role": "assistant", "content": "" }), None));
        }
    }

    /// Handle one upstream Anthropic SSE event, appending translated
    /// OpenAI SSE frames to `out`.
    pub fn on_event(&mut self, ev: &SseEvent, out: &mut Vec<String>) {
        if self.finished {
            return;
        }
        let name = ev.event.clone().unwrap_or_default();
        if name == "ping" {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
            return;
        };
        if name == "error" || v.get("type").and_then(|x| x.as_str()) == Some("error") {
            let msg = v
                .pointer("/error/message")
                .and_then(|x| x.as_str())
                .unwrap_or("upstream error");
            out.push(format!(
                "data: {}\n\n",
                json!({ "error": { "message": msg, "type": "upstream_error" } })
            ));
            out.push("data: [DONE]\n\n".to_string());
            self.finished = true;
            return;
        }
        match name.as_str() {
            "message_start" => {
                self.input_tokens = v
                    .pointer("/message/usage/input_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0)
                    + v.pointer("/message/usage/cache_read_input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0)
                    + v.pointer("/message/usage/cache_creation_input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                self.ensure_start(out);
            }
            "content_block_start" => {
                let bt = v
                    .pointer("/content_block/type")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                self.ensure_start(out);
                if bt == "tool_use" {
                    self.tool_seq += 1;
                    let idx = self.tool_seq - 1;
                    self.cur_tool = Some(idx);
                    let id = v
                        .pointer("/content_block/id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("toolu_0")
                        .to_string();
                    let nm = v
                        .pointer("/content_block/name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    out.push(self.chunk(
                        json!({
                            "tool_calls": [{
                                "index": idx,
                                "id": id,
                                "type": "function",
                                "function": { "name": nm, "arguments": "" },
                            }],
                        }),
                        None,
                    ));
                }
            }
            "content_block_delta" => {
                let dt = v
                    .pointer("/delta/type")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                match dt {
                    "text_delta" => {
                        if let Some(t) = v.pointer("/delta/text").and_then(|x| x.as_str()) {
                            self.ensure_start(out);
                            out.push(self.chunk(json!({ "content": t }), None));
                        }
                    }
                    "input_json_delta" => {
                        if let (Some(t), Some(idx)) = (
                            v.pointer("/delta/partial_json").and_then(|x| x.as_str()),
                            self.cur_tool,
                        ) {
                            out.push(self.chunk(
                                json!({
                                    "tool_calls": [{ "index": idx, "function": { "arguments": t } }],
                                }),
                                None,
                            ));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(t) = v.pointer("/delta/thinking").and_then(|x| x.as_str()) {
                            self.ensure_start(out);
                            out.push(self.chunk(json!({ "reasoning_content": t }), None));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                self.output_tokens = v
                    .pointer("/usage/output_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(self.output_tokens);
                if let Some(r) = v.pointer("/delta/stop_reason").and_then(|x| x.as_str()) {
                    self.finish_reason = Some(finish_reason(Some(r)).to_string());
                }
            }
            "message_stop" => {
                self.end(out);
            }
            _ => {}
        }
    }

    /// Emit the terminal frames (finish chunk, optional usage chunk,
    /// [DONE]). Safe to call more than once.
    pub fn end(&mut self, out: &mut Vec<String>) {
        if self.finished {
            return;
        }
        let fr = self
            .finish_reason
            .clone()
            .unwrap_or_else(|| "stop".to_string());
        self.ensure_start(out);
        out.push(self.chunk(json!({}), Some(&fr)));
        if self.include_usage {
            let obj = json!({
                "id": self.chat_id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.alias,
                "choices": [],
                "usage": {
                    "prompt_tokens": self.input_tokens,
                    "completion_tokens": self.output_tokens,
                    "total_tokens": self.input_tokens + self.output_tokens,
                },
            });
            out.push(format!("data: {}\n\n", obj));
        }
        out.push("data: [DONE]\n\n".to_string());
        self.finished = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_translation_basic() {
        let body = json!({
            "model": "k3",
            "messages": [
                { "role": "system", "content": "You are terse." },
                { "role": "user", "content": "Hi" },
            ],
        });
        let out = translate_request(&body, 8192).unwrap();
        assert_eq!(out["model"], "k3");
        assert_eq!(out["system"], "You are terse.");
        assert_eq!(out["max_tokens"], 8192);
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"][0]["text"], "Hi");
    }

    #[test]
    fn request_translation_tools_and_choice() {
        let body = json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "temp in Brussels" }],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "d",
                    "parameters": { "type": "object", "properties": {} },
                },
            }],
            "tool_choice": { "type": "function", "function": { "name": "get_weather" } },
            "stop": "END",
        });
        let out = translate_request(&body, 100).unwrap();
        assert_eq!(out["tools"][0]["name"], "get_weather");
        assert_eq!(out["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(
            out["tool_choice"],
            json!({ "type": "tool", "name": "get_weather" })
        );
        assert_eq!(out["stop_sequences"], json!(["END"]));
    }

    #[test]
    fn request_translation_tool_call_roundtrip() {
        let body = json!({
            "model": "m",
            "messages": [
                { "role": "user", "content": "call it" },
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "get_weather", "arguments": "{\"city\":\"Brussels\"}" },
                    }],
                },
                { "role": "tool", "tool_call_id": "call_1", "content": "18C" },
            ],
        });
        let out = translate_request(&body, 100).unwrap();
        let assistant = &out["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0]["type"], "tool_use");
        assert_eq!(assistant["content"][0]["id"], "call_1");
        assert_eq!(
            assistant["content"][0]["input"],
            json!({ "city": "Brussels" })
        );
        let tool = &out["messages"][2];
        assert_eq!(tool["role"], "user");
        assert_eq!(tool["content"][0]["type"], "tool_result");
        assert_eq!(tool["content"][0]["tool_use_id"], "call_1");
        assert_eq!(tool["content"][0]["content"][0]["text"], "18C");
    }

    #[test]
    fn response_translation_with_tool_use() {
        let upstream = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [
                { "type": "text", "text": "Checking." },
                {
                    "type": "tool_use",
                    "id": "toolu_9",
                    "name": "search",
                    "input": { "q": "nick" },
                },
            ],
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 11, "output_tokens": 9, "cache_read_input_tokens": 4 },
        });
        let out = translate_response(&upstream, "kimi-for-coding");
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["model"], "kimi-for-coding");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(out["choices"][0]["message"]["content"], "Checking.");
        let tc = &out["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["id"], "toolu_9");
        assert_eq!(tc["function"]["name"], "search");
        assert_eq!(tc["function"]["arguments"], "{\"q\":\"nick\"}");
        // input 11 + cache read 4
        assert_eq!(out["usage"]["prompt_tokens"], 15);
        assert_eq!(out["usage"]["completion_tokens"], 9);
    }

    #[test]
    fn stream_translation_text_and_tool() {
        let mut tlx = StreamTlx::new("k3", true);
        let mut out: Vec<String> = vec![];
        let events = [
            (
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":11,"cache_read_input_tokens":3}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            ("ping", r#"{"type":"ping"}"#),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_9","name":"search","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"nick\"}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":1}"#,
            ),
            (
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ];
        for (name, data) in events {
            tlx.on_event(
                &SseEvent {
                    event: Some(name.into()),
                    data: data.into(),
                },
                &mut out,
            );
        }
        let stream = out.join("");
        assert!(stream.contains("\"content\":\"Hel\""));
        assert!(stream.contains("\"content\":\"lo\""));
        assert!(stream.contains("\"id\":\"toolu_9\""));
        assert!(stream.contains("\"name\":\"search\""));
        assert!(stream.contains("\"finish_reason\":\"tool_calls\""));
        assert!(stream.contains("data: [DONE]"));

        // Reassemble the tool call arguments from the delta frames.
        let mut args = String::new();
        for frame in &out {
            if let Some(payload) = frame.strip_prefix("data: ").map(|s| s.trim()) {
                if let Ok(v) = serde_json::from_str::<Value>(payload) {
                    if let Some(tc) = v.pointer("/choices/0/delta/tool_calls/0") {
                        if let Some(a) = tc.pointer("/function/arguments").and_then(|x| x.as_str())
                        {
                            args.push_str(a);
                        }
                    }
                }
            }
        }
        assert_eq!(args, "{\"q\":\"nick\"}");

        // Usage chunk must carry input 11+3 and output 9.
        let usage_frames: Vec<&str> = out
            .iter()
            .filter(|f| f.contains("\"prompt_tokens\""))
            .map(|s| s.as_str())
            .collect();
        assert_eq!(usage_frames.len(), 1);
        assert!(usage_frames[0].contains("\"prompt_tokens\":14"));
        assert!(usage_frames[0].contains("\"completion_tokens\":9"));

        assert_eq!(tlx.input_tokens, 14);
        assert_eq!(tlx.output_tokens, 9);
    }

    #[test]
    fn stream_translation_error_event() {
        let mut tlx = StreamTlx::new("m", false);
        let mut out: Vec<String> = vec![];
        tlx.on_event(
            &SseEvent {
                event: Some("error".into()),
                data:
                    r#"{"type":"error","error":{"type":"overloaded_error","message":"try again"}}"#
                        .into(),
            },
            &mut out,
        );
        assert!(out[0].contains("\"error\""));
        assert!(out[0].contains("try again"));
        assert_eq!(out[1], "data: [DONE]\n\n");
        assert!(tlx.finished);
    }
}
