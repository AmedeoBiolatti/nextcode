//! LSP base protocol framing: `Content-Length` headers followed by a JSON body.

use std::io::{self, BufRead, Write};

/// Reads one message body. Returns `None` on a clean end of stream.
pub fn read_message(reader: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut length = None;
    let mut saw_header = false;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return if saw_header {
                Err(io::ErrorKind::UnexpectedEof.into())
            } else {
                Ok(None)
            };
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if let Some(length) = length {
                let mut body = vec![0; length];
                reader.read_exact(&mut body)?;
                return Ok(Some(body));
            }
            // Tolerate stray blank lines between messages.
            continue;
        }
        saw_header = true;
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid(format!("malformed header {line:?}")))?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            let value = value.trim();
            length = Some(
                value
                    .parse()
                    .map_err(|_| invalid(format!("bad Content-Length {value:?}")))?,
            );
        }
    }
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

    #[test]
    fn reads_back_to_back_messages_with_extra_headers() {
        let stream = b"Content-Length: 2\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}\
content-length:  7\r\n\r\n[1,2,3]";
        let mut reader = &stream[..];
        assert_eq!(read_message(&mut reader).unwrap().unwrap(), b"{}");
        assert_eq!(read_message(&mut reader).unwrap().unwrap(), b"[1,2,3]");
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn truncated_stream_is_an_error_not_a_clean_end() {
        let mut headers_only = &b"Content-Length: 10\r\n"[..];
        assert!(read_message(&mut headers_only).is_err());
        let mut short_body = &b"Content-Length: 10\r\n\r\n{}"[..];
        assert!(read_message(&mut short_body).is_err());
    }
}
