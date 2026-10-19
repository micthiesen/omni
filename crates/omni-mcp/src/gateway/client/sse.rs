//! Incremental `text/event-stream` decoder (WHATWG event-stream parsing for the
//! fields MCP uses: `event`, `data`, `id`; `retry` and comments are ignored).

/// One dispatched event. Events without data are not dispatched (per spec), which
/// also skips Streamable HTTP priming events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

impl SseEvent {
    /// MCP messages travel as the default `message` event type.
    pub fn is_message(&self) -> bool {
        matches!(self.event.as_deref(), None | Some("message"))
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("event stream line exceeds {0} bytes")]
pub struct SseTooLarge(pub usize);

#[derive(Debug)]
pub struct SseDecoder {
    limit: usize,
    line: Vec<u8>,
    pending_cr: bool,
    event: Option<String>,
    data: String,
    has_data: bool,
    id: Option<String>,
}

impl SseDecoder {
    /// `limit` bounds a single line and a single event's data.
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            line: Vec::new(),
            pending_cr: false,
            event: None,
            data: String::new(),
            has_data: false,
            id: None,
        }
    }

    /// Feeds bytes, returning the events completed by them.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseTooLarge> {
        let mut events = Vec::new();
        for &byte in chunk {
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            match byte {
                b'\n' => self.end_line(&mut events)?,
                b'\r' => {
                    self.pending_cr = true;
                    self.end_line(&mut events)?;
                }
                _ => {
                    if self.line.len() >= self.limit {
                        return Err(SseTooLarge(self.limit));
                    }
                    self.line.push(byte);
                }
            }
        }
        Ok(events)
    }

    fn end_line(&mut self, events: &mut Vec<SseEvent>) -> Result<(), SseTooLarge> {
        let line = std::mem::take(&mut self.line);
        let line = String::from_utf8_lossy(&line);
        if line.is_empty() {
            if self.has_data {
                events.push(SseEvent {
                    event: self.event.take(),
                    data: std::mem::take(&mut self.data),
                    id: self.id.clone(),
                });
            }
            self.event = None;
            self.data.clear();
            self.has_data = false;
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = match line.find(':') {
            Some(at) => {
                let value = &line[at + 1..];
                (&line[..at], value.strip_prefix(' ').unwrap_or(value))
            }
            None => (line.as_ref(), ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
                if self.data.len() > self.limit {
                    return Err(SseTooLarge(self.limit));
                }
            }
            "id" if !value.contains('\0') => self.id = Some(value.to_owned()),
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_split_chunks_multiline_data_and_line_endings() {
        let mut decoder = SseDecoder::new(1024);
        let mut events = decoder.push(b"id: 1\r\nevent: message\r\nda").unwrap();
        assert!(events.is_empty());
        events.extend(decoder.push(b"ta: {\"a\":\r\ndata: 1}\r\n\r").unwrap());
        events.extend(decoder.push(b"\n: comment\ndata:x\n\n").unwrap());
        assert_eq!(
            events,
            vec![
                SseEvent {
                    event: Some("message".into()),
                    data: "{\"a\":\n1}".into(),
                    id: Some("1".into()),
                },
                SseEvent {
                    event: None,
                    data: "x".into(),
                    id: Some("1".into())
                },
            ]
        );
    }

    #[test]
    fn multibyte_characters_split_across_chunks_survive() {
        let mut decoder = SseDecoder::new(1024);
        let text = "data: é✓\n\n".as_bytes();
        let mut events = decoder.push(&text[..7]).unwrap();
        events.extend(decoder.push(&text[7..]).unwrap());
        assert_eq!(events[0].data, "é✓");
    }

    #[test]
    fn bounds_lines_and_events() {
        let mut decoder = SseDecoder::new(8);
        assert_eq!(decoder.push(b"data: 0123456789"), Err(SseTooLarge(8)));
        let mut decoder = SseDecoder::new(8);
        assert_eq!(decoder.push(b"data:1234\ndata:5678\n"), Err(SseTooLarge(8)));
    }
}
