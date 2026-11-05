//! Minimal incremental SSE parser. Feeds receive arbitrary byte chunks;
//! `next_event` returns each complete `event:`/`data:` frame.

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug)]
pub struct SseParser {
    buf: Vec<u8>,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append bytes to the internal buffer.
    pub fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Pop the next complete event, if the buffer holds one.
    pub fn next_event(&mut self) -> Option<SseEvent> {
        loop {
            let end = find_event_end(&self.buf)?;
            let raw = String::from_utf8_lossy(&self.buf[..end]).into_owned();
            self.buf.drain(..end);
            let mut event: Option<String> = None;
            let mut data: String = String::new();
            for line in raw.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event = Some(rest.trim().to_string());
                } else if let Some(rest) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(rest.trim_start());
                }
                // comment lines ("..."), `id:`, `retry:` are ignored
            }
            if event.is_none() && data.is_empty() {
                continue;
            }
            return Some(SseEvent { event, data });
        }
    }
}

/// Find the end of the first complete event ("\n\n" or "\r\n\r\n").
fn find_event_end(buf: &[u8]) -> Option<usize> {
    let n = buf.len();
    for i in 0..n {
        if i + 2 <= n && &buf[i..i + 2] == b"\n\n" {
            return Some(i + 2);
        }
        if i + 4 <= n && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
    }
    None
}

/// Extract (prompt_tokens, completion_tokens) from an OpenAI-format body
/// or chunk that carries a `usage` object.
pub fn openai_usage(data: &str) -> Option<(u64, u64)> {
    let v: Value = serde_json::from_str(data).ok()?;
    let u = v.get("usage")?;
    let pt = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    let ct = u
        .get("completion_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    Some((pt, ct))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVENT_A: &str = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n";
    const EVENT_B: &str = "data: {\"type\":\"ping\"}\n\n";

    #[test]
    fn parses_single_event() {
        let mut p = SseParser::new();
        p.feed(EVENT_A.as_bytes());
        let ev = p.next_event().unwrap();
        assert_eq!(ev.event.as_deref(), Some("message_start"));
        assert_eq!(ev.data, "{\"type\":\"message_start\"}");
        assert!(p.next_event().is_none());
    }

    #[test]
    fn parses_split_across_chunks() {
        let full = format!("{EVENT_A}{EVENT_B}");
        // Split in the middle of the first event.
        let (a, b) = full.split_at(15);
        let mut p = SseParser::new();
        p.feed(a.as_bytes());
        assert!(p.next_event().is_none());
        p.feed(b.as_bytes());
        let ev1 = p.next_event().unwrap();
        assert_eq!(ev1.event.as_deref(), Some("message_start"));
        let ev2 = p.next_event().unwrap();
        assert_eq!(ev2.event, None);
        assert_eq!(ev2.data, "{\"type\":\"ping\"}");
        assert!(p.next_event().is_none());
    }

    #[test]
    fn crlf_line_endings() {
        let mut p = SseParser::new();
        p.feed(b"event: x\r\ndata: hello\r\n\r\n");
        let ev = p.next_event().unwrap();
        assert_eq!(ev.event.as_deref(), Some("x"));
        assert_eq!(ev.data, "hello");
    }

    #[test]
    fn multi_line_data() {
        let mut p = SseParser::new();
        p.feed(b"data: line1\ndata: line2\n\n");
        let ev = p.next_event().unwrap();
        assert_eq!(ev.data, "line1\nline2");
    }

    #[test]
    fn usage_extraction() {
        assert_eq!(
            openai_usage(r#"{"usage":{"prompt_tokens":3,"completion_tokens":2}}"#),
            Some((3, 2))
        );
        assert_eq!(openai_usage(r#"{"id":"x"}"#), None);
        assert_eq!(openai_usage("not json"), None);
    }
}
