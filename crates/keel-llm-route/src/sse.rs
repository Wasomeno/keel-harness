//! Server-sent events framing.
//!
//! This is transport, not protocol: it knows lines, fields and blank-line
//! dispatch, and nothing about JSON.

/// One dispatched SSE event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SseFrame {
    pub event: Option<String>,
    /// Multi-line `data:` fields joined with `\n`, as the SSE spec requires.
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseDecoder {
    buffer: Vec<u8>,
    event: Option<String>,
    data: String,
    has_data: bool,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes; returns every frame completed by them.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseFrame> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        let mut consumed = 0usize;

        while let Some(offset) = self.buffer[consumed..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = consumed + offset;
            let mut line = &self.buffer[consumed..end];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            let line = String::from_utf8_lossy(line).into_owned();
            consumed = end + 1;
            self.line(&line, &mut frames);
        }

        if consumed > 0 {
            self.buffer.drain(..consumed);
        }
        frames
    }

    /// Flush at end of stream. A trailing line without a newline is processed.
    pub fn finish(&mut self) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        if !self.buffer.is_empty() {
            let line = String::from_utf8_lossy(&self.buffer)
                .trim_end_matches('\r')
                .to_owned();
            self.buffer.clear();
            self.line(&line, &mut frames);
        }
        if self.has_data {
            frames.push(self.take_frame());
        }
        frames
    }

    fn line(&mut self, line: &str, frames: &mut Vec<SseFrame>) {
        if line.is_empty() {
            if self.has_data {
                frames.push(self.take_frame());
            }
            self.event = None;
            return;
        }

        if line.starts_with(':') {
            return;
        }

        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };

        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            // `id` and `retry` are not needed for model streams.
            _ => {}
        }
    }

    fn take_frame(&mut self) -> SseFrame {
        self.has_data = false;
        SseFrame {
            event: self.event.take(),
            data: std::mem::take(&mut self.data),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_on_blank_line() {
        let mut decoder = SseDecoder::new();
        let frames = decoder.push(b"data: one\n\ndata: two\n\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, "one");
        assert_eq!(frames[1].data, "two");
    }

    #[test]
    fn handles_crlf() {
        let mut decoder = SseDecoder::new();
        let frames = decoder.push(b"event: delta\r\ndata: hi\r\n\r\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].event.as_deref(), Some("delta"));
        assert_eq!(frames[0].data, "hi");
    }

    #[test]
    fn joins_multiline_data() {
        let mut decoder = SseDecoder::new();
        let frames = decoder.push(b"data: line one\ndata: line two\n\n");
        assert_eq!(frames[0].data, "line one\nline two");
    }

    #[test]
    fn survives_arbitrary_chunk_splits() {
        let text = b"event: delta\ndata: hello\n\ndata: world\n\n";
        let mut decoder = SseDecoder::new();
        let mut frames = Vec::new();
        for chunk in text.chunks(3) {
            frames.extend(decoder.push(chunk));
        }
        frames.extend(decoder.finish());
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].event.as_deref(), Some("delta"));
        assert_eq!(frames[0].data, "hello");
        assert_eq!(frames[1].data, "world");
    }

    #[test]
    fn ignores_comments_and_keeps_data() {
        let mut decoder = SseDecoder::new();
        let frames = decoder.push(b": keep-alive\ndata: value\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "value");
    }

    #[test]
    fn flushes_a_trailing_frame_without_a_blank_line() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.push(b"data: tail").is_empty());
        let frames = decoder.finish();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "tail");
    }

    #[test]
    fn an_empty_data_field_is_still_a_frame() {
        let mut decoder = SseDecoder::new();
        let frames = decoder.push(b"data:\n\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "");
    }
}
