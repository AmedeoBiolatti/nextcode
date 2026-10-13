//! JSON with comments and trailing commas, as `devcontainer.json` allows.

/// `text` as strict JSON: `//` and `/* */` comments become spaces (so
/// error positions still point at the original text) and commas before a
/// closing `}` or `]` are dropped.
pub fn to_json(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // Copy a string verbatim, escapes included.
                out.push(b'"');
                i += 1;
                while i < bytes.len() {
                    out.push(bytes[i]);
                    match bytes[i] {
                        b'\\' if i + 1 < bytes.len() => {
                            out.push(bytes[i + 1]);
                            i += 2;
                        }
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    out.push(b' ');
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                out.extend_from_slice(b"  ");
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                    i += 1;
                }
                if i < bytes.len() {
                    out.extend_from_slice(b"  ");
                    i += 2;
                }
            }
            b',' => {
                // A trailing comma: only whitespace (or comments, already
                // blanked when we get there) up to `}` or `]`.
                let mut j = i + 1;
                loop {
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if bytes.get(j) == Some(&b'/') && bytes.get(j + 1) == Some(&b'/') {
                        while j < bytes.len() && bytes[j] != b'\n' {
                            j += 1;
                        }
                    } else if bytes.get(j) == Some(&b'/') && bytes.get(j + 1) == Some(&b'*') {
                        j += 2;
                        while j < bytes.len()
                            && !(bytes[j] == b'*' && bytes.get(j + 1) == Some(&b'/'))
                        {
                            j += 1;
                        }
                        j += 2;
                    } else {
                        break;
                    }
                }
                out.push(if matches!(bytes.get(j), Some(b'}' | b']')) {
                    b' '
                } else {
                    b','
                });
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    // Only ASCII bytes were replaced, by ASCII, so this is still UTF-8.
    String::from_utf8(out).expect("UTF-8 preserved")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_trailing_commas_are_removed_but_strings_kept() {
        let text = r#"{
            // a comment, with "quotes"
            "url": "http://x//y", /* block
            comment */ "list": [1, 2, /* last */ ],
            "s": "a \" // not a comment",
        }"#;
        let value: serde_json::Value = serde_json::from_str(&to_json(text)).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "url": "http://x//y", "list": [1, 2], "s": "a \" // not a comment" })
        );
    }
}
