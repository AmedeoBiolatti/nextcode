//! Rust authoring support for the import-free NextCode JSON/WASM ABI.
//!
//! Pass a `fn(Request) -> Response` to [`export_extension!`], then compile a
//! `cdylib` for `wasm32-unknown-unknown`. Every invocation gets a fresh instance.
//! All document and edit offsets are UTF-8 byte offsets, not character indices.
//!
//! Panels are built with the constructors and setters on [`Widget`], [`ChartSpec`] and [`ChartSeries`]:
//!
//! ```
//! use extension_sdk::*;
//! let page = Widget::row("work", [
//!     Widget::column("runs", [Widget::table("t", vec!["run".into()], vec![vec!["r1".into()]])
//!         .selectable(None)]).width(320),
//!     Widget::column("charts", [Widget::grid("g", 360, [
//!         Widget::chart("loss", "val/loss", ChartSpec::line([
//!             ChartSeries::new("r1", [ChartPoint::new(1., 0.9), ChartPoint::new(2., 0.7)]).color(0),
//!         ]).scales(Scale::Linear, Scale::Log)).legend(false).sync("curves"),
//!         Widget::stat("best", "Best score", "0.812").detail("r1"),
//!     ])]).grow(),
//! ]);
//! assert_eq!(page.children().len(), 2);
//! ```

pub use extension_api;
pub use extension_api::*;

/// Export `nextcode_alloc(i32) -> i32` and `nextcode_run(i32, i32) -> i64`.
///
/// The host writes UTF-8 JSON into the allocation returned by `nextcode_alloc`.
/// `nextcode_run` accepts only that allocation and its exact original length.
/// Its return value packs the response pointer into the low 32 bits and response
/// length into the high 32 bits. All allocations remain guest-owned; the host
/// must copy the output before discarding the instance. No deallocator is needed.
///
/// The ABI exports are emitted only on wasm32. Rust's wasm32-unknown-unknown
/// target exports linear memory; neither this macro nor the SDK imports WASI.
#[macro_export]
macro_rules! export_extension {
    ($handler:path) => {
        #[cfg(target_arch = "wasm32")]
        static __NEXTCODE_MEMORY: ::std::sync::Mutex<$crate::__private::GuestMemory> =
            ::std::sync::Mutex::new($crate::__private::GuestMemory::new());

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn nextcode_alloc(len: i32) -> i32 {
            let mut memory = __NEXTCODE_MEMORY
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            memory.alloc(len)
        }

        #[cfg(target_arch = "wasm32")]
        #[unsafe(no_mangle)]
        pub extern "C" fn nextcode_run(ptr: i32, len: i32) -> i64 {
            let mut memory = __NEXTCODE_MEMORY
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            memory.run(ptr, len, $handler)
        }
    };
}

#[doc(hidden)]
#[cfg(any(target_arch = "wasm32", test))]
pub mod __private {
    use super::{
        Request, Response, API_VERSION, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, MIN_API_VERSION,
    };
    use std::io::{self, Write};

    fn error(message: &str) -> Vec<u8> {
        // These are bounded SDK-owned messages; Response contains no fallible
        // custom serializers and all strings are valid UTF-8.
        serde_json::to_vec(&Response {
            error: Some(message.to_owned()),
            ..Response::default()
        })
        .expect("serializing an SDK error response")
    }

    struct BoundedOutput(Vec<u8>);

    impl Write for BoundedOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_RESPONSE_BYTES - self.0.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "response too large",
                ));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn process_request(input: &[u8], handler: fn(Request) -> Response) -> Vec<u8> {
        if input.len() > MAX_REQUEST_BYTES {
            return error("Extension request exceeds the 16 MiB limit");
        }
        let request: Request = match serde_json::from_slice(input) {
            Ok(request) => request,
            Err(_) => {
                return error(
                    "Extension request is not valid UTF-8 JSON matching the request schema",
                );
            }
        };
        // The host sends the version the manifest declares; this SDK speaks all of them.
        if !(MIN_API_VERSION..=API_VERSION).contains(&request.api_version) {
            return error("Unsupported extension API version");
        }
        if let Some(document) = &request.document {
            let start = document.selection.start as usize;
            let end = document.selection.end as usize;
            if start > end
                || end > document.text.len()
                || !document.text.is_char_boundary(start)
                || !document.text.is_char_boundary(end)
            {
                return error(
                    "Document selection must contain ordered UTF-8 byte boundaries within the document",
                );
            }
        }
        let response = handler(request);
        let mut output = BoundedOutput(Vec::new());
        if serde_json::to_writer(&mut output, &response).is_err() {
            return error("Extension response exceeds the 1 MiB limit or could not be serialized");
        }
        output.0
    }

    #[cfg(target_arch = "wasm32")]
    pub struct GuestMemory {
        input: Option<Vec<u8>>,
        output: Option<Vec<u8>>,
    }

    #[cfg(target_arch = "wasm32")]
    impl GuestMemory {
        pub const fn new() -> Self {
            Self {
                input: None,
                output: None,
            }
        }

        pub fn alloc(&mut self, len: i32) -> i32 {
            if len <= 0 || len as usize > MAX_REQUEST_BYTES {
                self.input = None;
                return 0;
            }
            // Initialized storage ensures malformed host input never exposes
            // uninitialized Rust memory. Ownership is never rebuilt from a
            // host-supplied pointer or length.
            let input = vec![0; len as usize];
            let ptr = input.as_ptr() as usize as i32;
            self.input = Some(input);
            ptr
        }

        pub fn run(&mut self, ptr: i32, len: i32, handler: fn(Request) -> Response) -> i64 {
            let output = match self.input.take() {
                Some(input)
                    if len > 0
                        && len as usize == input.len()
                        && ptr == input.as_ptr() as usize as i32 =>
                {
                    process_request(&input, handler)
                }
                _ => error("Request pointer and length do not match the guest input allocation"),
            };
            let packed = ((output.len() as u64) << 32) | (output.as_ptr() as usize as u32 as u64);
            self.output = Some(output);
            packed as i64
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn must_not_run(_: Request) -> Response {
            panic!("malformed requests must not reach the handler");
        }

        fn assert_error(input: &[u8]) {
            let output = process_request(input, must_not_run);
            let response: Response = serde_json::from_slice(&output).unwrap();
            assert!(response.error.is_some());
            assert!(response.panels.is_empty());
            assert!(response.edits.is_empty());
        }

        #[test]
        fn malformed_requests_return_serializable_errors() {
            for input in [b"".as_slice(), b"{", b"\xff", b"{}", b"null"] {
                assert_error(input);
            }
            assert_error(&vec![b' '; MAX_REQUEST_BYTES + 1]);
        }

        #[test]
        fn unsupported_api_and_invalid_selection_do_not_reach_handler() {
            assert_error(br#"{"api_version":1,"invocation":{"kind":"command","id":"uppercase-selection"},"document":null,"settings":{}}"#);
            assert_error(br#"{"api_version":2,"invocation":{"kind":"command","id":"uppercase-selection"},"document":{"version":1,"path":null,"text":"abc","selection":{"start":2,"end":1}},"settings":{}}"#);
            assert_error("{\"api_version\":2,\"invocation\":{\"kind\":\"command\",\"id\":\"uppercase-selection\"},\"document\":{\"version\":1,\"path\":null,\"text\":\"é\",\"selection\":{\"start\":1,\"end\":2}},\"settings\":{}}".as_bytes());
        }

        #[test]
        fn oversized_response_is_replaced_by_error_without_edits() {
            let input = br#"{"api_version":2,"invocation":{"kind":"command","id":"uppercase-selection"},"document":null,"settings":{}}"#;
            let output = process_request(input, |_| Response {
                logs: vec!["x".repeat(MAX_RESPONSE_BYTES)],
                ..Response::default()
            });
            let response: Response = serde_json::from_slice(&output).unwrap();
            assert!(response.error.is_some());
            assert!(response.logs.is_empty());
            assert!(response.edits.is_empty());
            assert!(output.len() <= MAX_RESPONSE_BYTES);
        }
    }
}
