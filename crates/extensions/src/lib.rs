//! Restricted, import-free wasm32 extension packages.
//!
//! Each invocation gets a fresh instance, 10 million fuel (including start and
//! allocation), at most 64 MiB linear memory and one 16,384-element table. JSON
//! requests are limited to 16 MiB and responses to 1 MiB, manifests to 64 KiB and modules
//! to 16 MiB. Responses may contain at most 4,096 edits and 1,024 log entries.
//! Loading and installation never execute guest code, including start functions.
//! Loading checks declared resource minima statically; active data/element
//! initializer bounds are checked during execution-time instantiation.
//! Callers must run these blocking operations off the UI thread and check the
//! live document version again immediately before applying an entire edit batch.

mod package;
mod panels;
pub use panels::{validate_panel_event, validate_panel_update};

use anyhow::{Context, Result, bail, ensure};
use extension_api::{
    Document, Invocation, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, Manifest, Permission, Request,
    Response,
};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use wasmi::{
    Config, Engine, ExternType, FuncType, Linker, Module, Store, StoreLimits, StoreLimitsBuilder,
    ValType,
};

pub const MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;
const MAX_TABLE_ELEMENTS: usize = 16_384;
pub const MAX_FUEL: u64 = 10_000_000;
pub const MAX_EDITS: usize = 4096;
pub const MAX_LOGS: usize = 1024;

#[derive(Clone, Debug)]
pub struct Extension {
    pub manifest: Manifest,
    directory: PathBuf,
    module: Arc<Module>,
    // Keep the validated security policy separate from the public UI metadata.
    policy: Arc<Manifest>,
}

impl Extension {
    pub fn load(directory: &Path) -> Result<Self> {
        let dir = package::open_directory(directory, false)?;
        let (manifest, wasm) = package::read_package(&dir)?;
        Self::compile(directory.to_path_buf(), manifest, &wasm)
    }

    fn compile(directory: PathBuf, manifest: Manifest, wasm: &[u8]) -> Result<Self> {
        package::validate_manifest(&manifest)?;
        let mut config = Config::default();
        config.consume_fuel(true);
        // Extended-const expression trees recurse on the host stack outside
        // allocation ABI needs only ordinary constant initializers.
        config.wasm_extended_const(false);
        config.wasm_custom_page_sizes(false);
        config.wasm_multi_memory(false);
        let engine = Engine::new(&config);
        let module =
            Module::new(&engine, wasm).context("invalid or unsupported WebAssembly module")?;
        if let Some(import) = module.imports().next() {
            bail!(
                "extension imports are forbidden: {}::{}",
                import.module(),
                import.name()
            );
        }
        match module.get_export("memory") {
            Some(ExternType::Memory(memory)) => {
                ensure!(
                    memory.minimum() <= (MAX_MEMORY_BYTES / 65536) as u64,
                    "initial memory exceeds the 64 MiB limit"
                );
            }
            _ => bail!("extension must export linear memory named 'memory'"),
        }
        for (name, expected) in [
            (
                "nextcode_alloc",
                FuncType::new([ValType::I32], [ValType::I32]),
            ),
            (
                "nextcode_run",
                FuncType::new([ValType::I32, ValType::I32], [ValType::I64]),
            ),
        ] {
            ensure!(
                matches!(module.get_export(name), Some(ExternType::Func(actual)) if actual == expected),
                "missing or incorrect ABI export '{name}'"
            );
        }
        // No instantiation here: wasmi instantiation also executes guest start.
        validate_initial_resources(wasm)?;
        Ok(Self {
            directory,
            policy: Arc::new(manifest.clone()),
            manifest,
            module: Arc::new(module),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn install(source: &Path, root: &Path) -> Result<Self> {
        package::install(source, root)
    }

    pub fn discover(root: &Path) -> Vec<(PathBuf, Result<Self>)> {
        package::discover(root)
    }

    pub fn run(&self, mut request: Request) -> Result<Response> {
        ensure!(
            request.api_version == self.policy.api_version,
            "request API version {} is unsupported",
            request.api_version
        );
        match &request.invocation {
            Invocation::Command { id } => ensure!(
                self.policy.commands.iter().any(|c| c.id == *id),
                "unknown extension command"
            ),
            Invocation::OpenPanel { id } => ensure!(
                self.policy.panels.iter().any(|p| p.id == *id),
                "unknown extension panel"
            ),
            Invocation::UiEvent {
                panel,
                state,
                inputs,
                ..
            }
            | Invocation::Refresh {
                panel,
                state,
                inputs,
                ..
            }
            | Invocation::ProcessExit {
                panel,
                state,
                inputs,
                ..
            } => {
                ensure!(
                    self.policy.api_version >= 3
                        || matches!(request.invocation, Invocation::UiEvent { .. }),
                    "background invocations require API 3"
                );
                ensure!(
                    self.policy.panels.iter().any(|p| p.id == *panel),
                    "unknown extension panel"
                );
                panels::validate_state(state)?;
                ensure!(inputs.len() <= 128, "too many panel inputs");
                for (id, value) in inputs {
                    panels::validate_id(id)?;
                    if let extension_api::InputValue::Text(text) = value {
                        // Select option values use the aggregate panel text
                        // budget; actual text fields are checked against the
                        // current model's stricter 4 KiB limit by the host.
                        ensure!(text.len() <= 65536, "panel input exceeds 64 KiB");
                    }
                }
            }
        }
        if !self.policy.permissions.contains(&Permission::ReadDocument) {
            request.document = None;
        }
        if let Some(document) = &request.document {
            validate_range(
                &document.text,
                document.selection.start,
                document.selection.end,
            )
            .context("invalid document selection")?;
        }
        let mut input = BoundedJson(Vec::new());
        serde_json::to_writer(&mut input, &request)
            .context("request exceeds 16 MiB or cannot be encoded")?;
        let engine = self.module.engine();
        let mut store = limited_store(engine)?;
        let instance = Linker::<StoreLimits>::new(engine)
            .instantiate_and_start(&mut store, &self.module)
            .context("extension instantiation/start failed (invalid initializers, fuel/memory limit or trap)")?;
        let memory = instance
            .get_memory(&store, "memory")
            .context("missing guest memory")?;
        let alloc = instance.get_typed_func::<i32, i32>(&store, "nextcode_alloc")?;
        let run = instance.get_typed_func::<(i32, i32), i64>(&store, "nextcode_run")?;
        let len = input.0.len() as i32;
        let ptr = alloc
            .call(&mut store, len)
            .context("extension allocation failed (fuel/memory limit or trap)")?;
        // Interpret wasm32 addresses as unsigned; checked slices reject wrapping ranges.
        let offset = ptr as u32 as usize;
        let end = offset
            .checked_add(input.0.len())
            .context("input pointer overflow")?;
        ensure!(
            end <= memory.data(&store).len(),
            "extension returned an invalid input allocation pointer"
        );
        memory
            .write(&mut store, offset, &input.0)
            .map_err(|e| anyhow::anyhow!("cannot write guest request: {e}"))?;
        let packed = run
            .call(&mut store, (ptr, len))
            .context("extension execution failed (fuel/memory limit or trap)")?
            as u64;
        let output_ptr = packed as u32 as usize;
        let output_len = (packed >> 32) as usize;
        ensure!(
            output_len <= MAX_RESPONSE_BYTES,
            "extension response exceeds 1 MiB"
        );
        let output_end = output_ptr
            .checked_add(output_len)
            .context("output pointer overflow")?;
        let output = memory
            .data(&store)
            .get(output_ptr..output_end)
            .context("extension returned an invalid output pointer/length")?;
        let response: Response = serde_json::from_slice(output)
            .context("extension response must be valid UTF-8 protocol JSON")?;
        self.validate_reply(&request, &response)?;
        Ok(response)
    }
    /// Validate host-provided stdout exactly like a WASM reply. No effects are
    /// applied until this and the UI's live-origin checks have both succeeded.
    pub fn validate_reply(&self, request: &Request, response: &Response) -> Result<()> {
        validate_response(
            response,
            request
                .document
                .as_ref()
                .filter(|_| self.policy.permissions.contains(&Permission::ReadDocument)),
            &self.policy.permissions,
        )?;
        let mut ids = std::collections::HashSet::new();
        for panel in &response.panels {
            ensure!(
                self.policy.panels.iter().any(|p| p.id == panel.id),
                "response updates undeclared panel"
            );
            ensure!(ids.insert(&panel.id), "duplicate panel update");
            if let Invocation::UiEvent { panel: target, .. }
            | Invocation::Refresh { panel: target, .. }
            | Invocation::ProcessExit { panel: target, .. } = &request.invocation
            {
                ensure!(target == &panel.id, "event updates another panel");
            }
        }
        if response.error.is_none()
            && let Invocation::OpenPanel { id } = &request.invocation
        {
            ensure!(
                response.panels.iter().any(|p| &p.id == id)
                    || (self.policy.api_version >= 3 && !response.processes.is_empty()),
                "open-panel response omitted requested panel"
            );
        }

        ensure!(
            self.policy.api_version >= 3
                || (response.processes.is_empty()
                    && response.panels.iter().all(|p| p.refresh_ms.is_none())),
            "processes and timers require API 3"
        );
        if !response.processes.is_empty() {
            ensure!(
                self.policy.permissions.contains(&Permission::RunProcess),
                "extension lacks run-process permission"
            );
            ensure!(
                !matches!(request.invocation, Invocation::Command { .. }),
                "processes require a panel invocation"
            );
        }
        let mut ids = std::collections::HashSet::new();
        for process in &response.processes {
            panels::validate_id(&process.id)?;
            ensure!(ids.insert(&process.id), "duplicate process id");
            ensure!(
                self.policy
                    .processes
                    .iter()
                    .any(|p| p.program == process.program),
                "undeclared program"
            );
            ensure!(
                (1..=extension_api::MAX_PROCESS_TIMEOUT_MS).contains(&process.timeout_ms),
                "invalid process timeout"
            );
            ensure!(
                process.args.len() <= 256
                    && process
                        .args
                        .iter()
                        .all(|a| a.len() <= 65536 && !a.contains('\0')),
                "invalid or excessive process arguments"
            );
            ensure!(
                process.args.iter().map(String::len).sum::<usize>() <= 65536,
                "process arguments exceed 64 KiB"
            );
        }
        Ok(())
    }
}

fn validate_initial_resources(wasm: &[u8]) -> Result<()> {
    use wasmparser::{Parser, Payload};
    let mut memory_count = 0;
    let mut table_count = 0;
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.context("cannot inspect module resource declarations")? {
            Payload::MemorySection(memories) => {
                memory_count += memories.count();
                ensure!(
                    memory_count <= 1,
                    "extension may define only one linear memory"
                );
                for memory in memories {
                    let memory = memory?;
                    ensure!(
                        !memory.memory64 && !memory.shared && memory.page_size_log2.is_none(),
                        "extension requires ordinary unshared wasm32 memory with 64 KiB pages"
                    );
                    ensure!(
                        memory.initial <= (MAX_MEMORY_BYTES / 65536) as u64,
                        "initial memory exceeds the 64 MiB limit"
                    );
                }
            }
            Payload::TableSection(tables) => {
                table_count += tables.count();
                ensure!(table_count <= 1, "extension may define at most one table");
                for table in tables {
                    let table = table?;
                    ensure!(
                        table.ty.initial <= MAX_TABLE_ELEMENTS as u64,
                        "initial table exceeds the {MAX_TABLE_ELEMENTS}-element limit"
                    );
                }
            }
            _ => (),
        }
    }
    ensure!(memory_count == 1, "extension must define one linear memory");
    Ok(())
}

fn limited_store(engine: &Engine) -> Result<Store<StoreLimits>> {
    let limits = StoreLimitsBuilder::new()
        .memory_size(MAX_MEMORY_BYTES)
        .table_elements(MAX_TABLE_ELEMENTS)
        .memories(1)
        .tables(1)
        .instances(1)
        .trap_on_grow_failure(true)
        .build();
    let mut store = Store::new(engine, limits);
    store.limiter(|limits| limits);
    store.set_fuel(MAX_FUEL)?;
    Ok(store)
}

fn validate_range(text: &str, start: u32, end: u32) -> Result<()> {
    let (start, end) = (start as usize, end as usize);
    ensure!(
        start <= end && end <= text.len(),
        "range is reversed or outside the document"
    );
    ensure!(
        text.is_char_boundary(start) && text.is_char_boundary(end),
        "range splits a UTF-8 character"
    );
    Ok(())
}

/// Validate a response atomically against its original snapshot and permissions.
/// The caller must additionally compare `Document::version` to the live buffer.
pub fn validate_response(
    response: &Response,
    document: Option<&Document>,
    permissions: &[Permission],
) -> Result<()> {
    ensure!(
        response.edits.len() <= MAX_EDITS,
        "response exceeds {MAX_EDITS} edits"
    );
    ensure!(
        response.logs.len() <= MAX_LOGS,
        "response exceeds {MAX_LOGS} log entries"
    );
    panels::validate_encoded_size(response, MAX_RESPONSE_BYTES)?;
    ensure!(
        response.error.is_none()
            || (response.edits.is_empty()
                && response.panels.is_empty()
                && response.processes.is_empty()),
        "response contains updates together with error"
    );
    ensure!(
        response.processes.len() <= extension_api::MAX_PROCESSES,
        "too many processes"
    );
    ensure!(response.panels.len() <= 8, "too many panel updates");
    for panel in &response.panels {
        validate_panel_update(panel)?;
    }
    if response.edits.is_empty() {
        return Ok(());
    }
    ensure!(
        response.error.is_none(),
        "extension returned edits together with an error; no edits accepted"
    );
    ensure!(
        permissions.contains(&Permission::ReadDocument)
            && permissions.contains(&Permission::EditDocument),
        "extension lacks read-document and edit-document permissions; no edits accepted"
    );
    let document = document.context("extension returned edits without a document snapshot")?;
    let mut ranges = Vec::with_capacity(response.edits.len());
    for edit in &response.edits {
        validate_range(&document.text, edit.start, edit.end)
            .context("invalid extension edit; no edits accepted")?;
        ranges.push((edit.start, edit.end));
    }
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        ensure!(
            b.0 >= a.1 && !(a.0 == a.1 && a.0 == b.0),
            "overlapping edits or duplicate insertion; no edits accepted"
        );
    }
    Ok(())
}

struct BoundedJson(Vec<u8>);
impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_REQUEST_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("request JSON exceeds 16 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
use extension_api::API_VERSION;
#[cfg(test)]
mod tests;
