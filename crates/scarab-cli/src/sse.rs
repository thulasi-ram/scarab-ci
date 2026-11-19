//! A Server-Sent-Events parser for `scarab logs`, per the WHATWG event-stream
//! format: lines end in CRLF, LF or CR; `data:` lines of one event join with
//! `\n`; one leading space after the colon is stripped; `:` lines are comments;
//! a blank line dispatches the event.

/// One dispatched event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field; `None` = the default `message`.
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

/// Incremental parser: feed it bytes as they arrive, collect events.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data: String,
    has_data: bool,
    id: Option<String>,
}

impl SseParser {
    /// Feed bytes; returns the events completed by them. Bytes split anywhere
    /// (inside a line or a UTF-8 character) are held until the line ends.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut start = 0;
        let mut i = 0;
        while i < self.buf.len() {
            match self.buf[i] {
                b'\n' => {
                    let line = self.buf[start..i].to_vec();
                    self.line(&line, &mut out);
                    i += 1;
                    start = i;
                }
                b'\r' => {
                    // A CR at the end of what we have may be the first half of
                    // a CRLF: wait for the next byte.
                    if i + 1 == self.buf.len() {
                        break;
                    }
                    let line = self.buf[start..i].to_vec();
                    self.line(&line, &mut out);
                    i += if self.buf[i + 1] == b'\n' { 2 } else { 1 };
                    start = i;
                }
                _ => i += 1,
            }
        }
        self.buf.drain(..start);
        out
    }

    fn line(&mut self, line: &[u8], out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            if self.has_data {
                out.push(SseEvent {
                    event: self.event.take(),
                    data: std::mem::take(&mut self.data),
                    id: self.id.clone(),
                });
            }
            self.event = None;
            self.data.clear();
            self.has_data = false;
            return;
        }
        if line[0] == b':' {
            return;
        }
        let line = String::from_utf8_lossy(line);
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line.as_ref(), ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => self.event = Some(value.to_string()),
            "id" if !value.contains('\0') => self.id = Some(value.to_string()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut p = SseParser::default();
        chunks.iter().flat_map(|c| p.feed(c)).collect()
    }

    /// Two events whose payloads are the two halves of one log line: printed
    /// back to back they must give the line once, with no newline added at
    /// the event boundary.
    #[test]
    fn payloads_join_without_added_newlines() {
        // What axum writes for data "hello wo" and "rld\n".
        let wire: &[u8] = b"id: a1:1\ndata: hello wo\n\nid: a1:2\ndata: rld\ndata: \n\n";
        let events = parse_all(&[wire]);
        let printed: String = events.iter().map(|e| e.data.as_str()).collect();
        assert_eq!(printed, "hello world\n");
        assert_eq!(events[1].id.as_deref(), Some("a1:2"));
    }

    #[test]
    fn bytes_split_anywhere_parse_the_same() {
        let wire = "data: héllo\ndata:x\n\n: keepalive\n\nevent: error\r\ndata: boom\r\n\r\n";
        let whole = parse_all(&[wire.as_bytes()]);
        assert_eq!(
            whole,
            vec![
                SseEvent {
                    event: None,
                    data: "héllo\nx".into(),
                    id: None
                },
                SseEvent {
                    event: Some("error".into()),
                    data: "boom".into(),
                    id: None
                },
            ]
        );
        // Every split point, including inside `é` and between CR and LF.
        let bytes = wire.as_bytes();
        for cut in 0..bytes.len() {
            assert_eq!(
                parse_all(&[&bytes[..cut], &bytes[cut..]]),
                whole,
                "cut {cut}"
            );
        }
    }

    #[test]
    fn only_one_leading_space_is_stripped() {
        let events = parse_all(&[b"data:   indented\n\n"]);
        assert_eq!(events[0].data, "  indented");
    }
}
