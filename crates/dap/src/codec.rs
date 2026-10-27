//! DAP base protocol framing: `Content-Length` headers, a blank line, then
//! a JSON body (the same framing as LSP).

use std::io::{self, Write};

/// Splits a byte stream into message bodies, whatever the chunking: a
/// chunk may hold part of a header, several messages, or a message's tail.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
    /// Body length of the message whose headers were read.
    body_length: Option<usize>,
}

impl FrameDecoder {
    /// Adds `bytes` and returns every message they complete.
    pub fn push(&mut self, bytes: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        self.buffer.extend_from_slice(bytes);
        let mut messages = Vec::new();
        let mut start = 0;
        loop {
            match self.body_length {
                Some(length) => {
                    if self.buffer.len() - start < length {
                        break;
                    }
                    messages.push(self.buffer[start..start + length].to_vec());
                    start += length;
                    self.body_length = None;
                }
                None => {
                    let Some(end) = find(&self.buffer[start..], b"\r\n\r\n") else {
                        // Tolerate stray line breaks between messages.
                        while self.buffer[start..].starts_with(b"\r\n") {
                            start += 2;
                        }
                        break;
                    };
                    let headers = &self.buffer[start..start + end];
                    self.body_length = Some(content_length(headers)?);
                    start += end + 4;
                }
            }
        }
        self.buffer.drain(..start);
        Ok(messages)
    }

    /// Whether a message was cut off (bytes left over at end of stream).
    pub fn has_partial(&self) -> bool {
        self.body_length.is_some() || !self.buffer.is_empty()
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn content_length(headers: &[u8]) -> io::Result<usize> {
    let headers = std::str::from_utf8(headers).map_err(|_| invalid("non-UTF-8 headers".into()))?;
    for line in headers.split("\r\n").filter(|line| !line.trim().is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid(format!("malformed header {line:?}")))?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            let value = value.trim();
            return value
                .parse()
                .map_err(|_| invalid(format!("bad Content-Length {value:?}")));
        }
    }
    Err(invalid(format!("no Content-Length in {headers:?}")))
}

pub fn write_message(writer: &mut impl Write, body: &[u8]) -> io::Result<()> {
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(body)?;
    writer.flush()
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(body: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_message(&mut bytes, body.as_bytes()).unwrap();
        bytes
    }

    #[test]
    fn merged_frames_in_one_chunk_all_come_out() {
        let mut stream = frame(r#"{"seq":1}"#);
        stream.extend(frame(r#"{"seq":2}"#));
        stream.extend(b"Content-Length: 2\r\nContent-Type: application/json\r\n\r\n{}");
        let mut decoder = FrameDecoder::default();
        let messages = decoder.push(&stream).unwrap();
        assert_eq!(
            messages,
            [
                br#"{"seq":1}"#.to_vec(),
                br#"{"seq":2}"#.to_vec(),
                b"{}".to_vec()
            ]
        );
        assert!(!decoder.has_partial());
    }

    #[test]
    fn frames_split_anywhere_reassemble() {
        let mut stream = frame(r#"{"text":"héllo"}"#);
        stream.extend(frame(r#"{"seq":2}"#));
        // Every split point, including inside the header and the UTF-8 body.
        for split in 0..stream.len() {
            let mut decoder = FrameDecoder::default();
            let mut messages = decoder.push(&stream[..split]).unwrap();
            messages.extend(decoder.push(&stream[split..]).unwrap());
            assert_eq!(
                messages,
                [
                    r#"{"text":"héllo"}"#.as_bytes().to_vec(),
                    br#"{"seq":2}"#.to_vec()
                ],
                "split at {split}"
            );
        }
        // Byte by byte.
        let mut decoder = FrameDecoder::default();
        let mut messages = Vec::new();
        for byte in &stream {
            messages.extend(decoder.push(std::slice::from_ref(byte)).unwrap());
        }
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn cut_off_messages_are_reported_and_bad_headers_rejected() {
        let mut decoder = FrameDecoder::default();
        assert!(
            decoder
                .push(b"Content-Length: 10\r\n\r\n{}")
                .unwrap()
                .is_empty()
        );
        assert!(decoder.has_partial());
        assert!(
            FrameDecoder::default()
                .push(b"Content-Length: x\r\n\r\n")
                .is_err()
        );
        assert!(
            FrameDecoder::default()
                .push(b"Content-Type: a\r\n\r\n")
                .is_err()
        );
    }
}
