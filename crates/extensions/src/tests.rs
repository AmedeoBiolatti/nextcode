use super::*;
use extension_api::{Command, Selection, TextEdit};
use std::fs;
use tempfile::TempDir;

fn manifest() -> Manifest {
    Manifest {
        id: "test.extension".into(),
        name: "Test extension".into(),
        version: "1.0.0".into(),
        api_version: API_VERSION,
        module: "extension.wasm".into(),
        commands: vec![Command {
            id: "test.run".into(),
            title: "Run".into(),
        }],
        panels: Vec::new(),
        permissions: vec![Permission::ReadDocument, Permission::EditDocument],
        processes: Vec::new(),
    }
}

fn request() -> Request {
    Request {
        api_version: API_VERSION,
        invocation: Invocation::Command {
            id: "test.run".into(),
        },
        document: Some(Document {
            version: 42,
            path: Some("remote://file".into()),
            text: "aébc".into(),
            selection: Selection { start: 1, end: 3 },
        }),
        settings: serde_json::json!({"enabled": true}),
    }
}

fn wasm_response(bytes: &[u8], alloc: &str, run: Option<&str>, extra: &str) -> Vec<u8> {
    let data: String = bytes.iter().map(|b| format!("\\{b:02x}")).collect();
    let packed = ((bytes.len() as u64) << 32) | 1024;
    let default_run = format!("i64.const {packed}");
    wat::parse_str(format!(
        r#"(module
        (memory (export "memory") 64)
        (data (i32.const 1024) "{data}")
        (func (export "nextcode_alloc") (param i32) (result i32) {alloc})
        (func (export "nextcode_run") (param i32 i32) (result i64) {})
        {extra})"#,
        run.unwrap_or(&default_run)
    ))
    .unwrap()
}

fn compile(wasm: &[u8], manifest: Manifest) -> Result<Extension> {
    Extension::compile(PathBuf::from("test-package"), manifest, wasm)
}

fn normal(response: &str) -> Extension {
    compile(
        &wasm_response(response.as_bytes(), "i32.const 2097152", None, ""),
        manifest(),
    )
    .unwrap()
}

fn package(parent: &Path, name: &str, manifest: &Manifest, wasm: &[u8]) -> PathBuf {
    let dir = parent.join(name);
    fs::create_dir(&dir).unwrap();
    fs::write(
        dir.join("extension.json"),
        serde_json::to_vec(manifest).unwrap(),
    )
    .unwrap();
    fs::write(dir.join(&manifest.module), wasm).unwrap();
    dir
}

#[test]
fn forbidden_imports_and_wrong_abi_are_rejected() {
    let wasm =
        wat::parse_str(r#"(module (import "wasi_snapshot_preview1" "fd_write" (func)))"#).unwrap();
    assert!(
        format!("{:#}", compile(&wasm, manifest()).unwrap_err()).contains("imports are forbidden")
    );
    for source in [
        "(module)",
        r#"(module (memory (export "memory") 1) (func (export "nextcode_alloc") (result i64) i64.const 0))"#,
        r#"(module (memory (export "memory") 1025))"#,
    ] {
        assert!(compile(&wat::parse_str(source).unwrap(), manifest()).is_err());
    }
}

#[test]
fn load_never_runs_start_and_all_execution_phases_share_fuel_limits() {
    for (alloc, run, extra, stage) in [
        (
            "i32.const 2097152",
            None,
            "(func $start (loop $l br $l)) (start $start)",
            "start",
        ),
        ("(loop $l br $l) unreachable", None, "", "allocation"),
        (
            "i32.const 2097152",
            Some("(loop $l br $l) unreachable"),
            "",
            "execution",
        ),
    ] {
        let extension = compile(&wasm_response(b"{}", alloc, run, extra), manifest()).unwrap();
        let error = format!("{:#}", extension.run(request()).unwrap_err());
        assert!(error.contains(stage), "{error}");
        assert!(error.contains("fuel"), "{error}");
    }
    let extension = compile(
        &wasm_response(
            b"{}",
            "i32.const 2097152",
            None,
            "(func $start unreachable) (start $start)",
        ),
        manifest(),
    )
    .unwrap();
    assert!(extension.run(request()).is_err());
}

#[test]
fn growth_and_initial_tables_obey_resource_limits() {
    let extension = compile(
        &wasm_response(
            b"{}",
            "i32.const 2097152",
            Some("i32.const 1024 memory.grow drop i64.const 0"),
            "",
        ),
        manifest(),
    )
    .unwrap();
    assert!(format!("{:#}", extension.run(request()).unwrap_err()).contains("memory"));
    let wasm = wasm_response(b"{}", "i32.const 2097152", None, "(table 16385 funcref)");
    assert!(compile(&wasm, manifest()).is_err());
}

#[test]
fn invalid_guest_pointers_lengths_utf8_and_json_are_errors() {
    for (bytes, alloc, run) in [
        (b"{}".as_slice(), "i32.const -1", None),
        (b"{}".as_slice(), "i32.const 4194303", None),
        (
            b"{}".as_slice(),
            "i32.const 2097152",
            Some("i64.const 12884901887"),
        ),
        (
            b"{}".as_slice(),
            "i32.const 2097152",
            Some("i64.const 4503603922337792"),
        ),
        (b"\xff".as_slice(), "i32.const 2097152", None),
        (b"not json".as_slice(), "i32.const 2097152", None),
        (
            br#"{"edits":"wrong"}"#.as_slice(),
            "i32.const 2097152",
            None,
        ),
        (
            br#"{"unexpected":true}"#.as_slice(),
            "i32.const 2097152",
            None,
        ),
    ] {
        let extension = compile(&wasm_response(bytes, alloc, run, ""), manifest()).unwrap();
        assert!(extension.run(request()).is_err());
    }
}

#[test]
fn fresh_instance_for_every_call_and_defaults_work() {
    let wasm = wasm_response(
        b"{}",
        "i32.const 2097152",
        Some(
            "global.get $calls if unreachable end i32.const 1 global.set $calls i64.const 8589935616",
        ),
        "(global $calls (mut i32) (i32.const 0))",
    );
    let extension = compile(&wasm, manifest()).unwrap();
    assert_eq!(extension.run(request()).unwrap(), Response::default());
    assert_eq!(
        extension.clone().run(request()).unwrap(),
        Response::default()
    );
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Extension>();
}

#[test]
fn requests_validate_command_api_selection_and_bound_serialization() {
    let extension = normal("{}");
    let mut req = request();
    req.invocation = Invocation::Command { id: "other".into() };
    assert!(extension.run(req).is_err());
    let mut req = request();
    req.api_version += 1;
    assert!(extension.run(req).is_err());
    let mut req = request();
    req.document.as_mut().unwrap().selection.end = 2;
    assert!(extension.run(req).is_err());
    let mut req = request();
    req.settings = serde_json::Value::String("x".repeat(MAX_REQUEST_BYTES));
    assert!(format!("{:#}", extension.run(req).unwrap_err()).contains("16 MiB"));
    let mut req = request();
    req.settings = serde_json::Value::String("x".repeat(1024 * 1024));
    assert_eq!(extension.run(req).unwrap(), Response::default());
}

#[test]
fn document_is_redacted_without_read_permission_even_if_public_metadata_changes() {
    // Scan the JSON bytes for a document-only eight-byte marker, trapping if found.
    let marker = i64::from_le_bytes(*b"SECRETS!");
    let run = format!(
        r#"(local $i i32)
        local.get 0 local.set $i
        (block $done (loop $scan
            local.get $i i32.const 8 i32.add local.get 0 local.get 1 i32.add i32.gt_u br_if $done
            local.get $i i64.load i64.const {marker} i64.eq if unreachable end
            local.get $i i32.const 1 i32.add local.set $i br $scan))
        i64.const 8589935616"#
    );
    let wasm = wasm_response(b"{}", "i32.const 2097152", Some(&run), "");
    let mut policy = manifest();
    policy.permissions.clear();
    let mut extension = compile(&wasm, policy).unwrap();
    extension.manifest.permissions = vec![Permission::ReadDocument, Permission::EditDocument];
    let mut req = request();
    req.document.as_mut().unwrap().text = "SECRETS!".into();
    assert!(extension.run(req.clone()).is_ok());
    let reader = compile(&wasm, manifest()).unwrap();
    assert!(reader.run(req).is_err());
}

fn edit(start: u32, end: u32) -> TextEdit {
    TextEdit {
        start,
        end,
        text: "replacement".into(),
    }
}

#[test]
fn edit_batches_are_validated_atomically_against_utf8_snapshot() {
    let req = request();
    let permissions = manifest().permissions;
    let valid = Response {
        edits: vec![edit(3, 5), edit(0, 1)],
        ..Response::default()
    };
    assert!(validate_response(&valid, req.document.as_ref(), &permissions).is_ok());
    for edits in [
        vec![edit(0, 1), edit(1, 2)], // One invalid edit invalidates all, including first valid edit.
        vec![edit(0, 3), edit(1, 5)],
        vec![edit(1, 1), edit(1, 1)],
        vec![edit(1, 1), edit(1, 3)],
        vec![edit(3, 3), edit(1, 5)],
        vec![edit(4, 3)],
        vec![edit(0, 6)],
    ] {
        let response = Response {
            edits,
            ..Response::default()
        };
        assert!(validate_response(&response, req.document.as_ref(), &permissions).is_err());
        let extension = normal(&serde_json::to_string(&response).unwrap());
        assert!(extension.run(req.clone()).is_err());
    }
    assert!(validate_response(&valid, None, &permissions).is_err());
    assert!(validate_response(&valid, req.document.as_ref(), &[]).is_err());
    assert!(validate_response(&valid, req.document.as_ref(), &[Permission::ReadDocument]).is_err());
    assert!(validate_response(&valid, req.document.as_ref(), &[Permission::EditDocument]).is_err());
    let failed = Response {
        error: Some("guest failed".into()),
        ..valid.clone()
    };
    assert!(validate_response(&failed, req.document.as_ref(), &permissions).is_err());
    let extension = normal(&serde_json::to_string(&valid).unwrap());
    let mut req = request();
    req.document = None;
    assert!(extension.run(req).is_err());
}

#[test]
fn runtime_checks_edit_permissions_and_response_limits() {
    let response = Response {
        edits: vec![edit(0, 1)],
        ..Response::default()
    };
    let mut policy = manifest();
    policy.permissions = vec![Permission::ReadDocument];
    let extension = compile(
        &wasm_response(
            &serde_json::to_vec(&response).unwrap(),
            "i32.const 2097152",
            None,
            "",
        ),
        policy,
    )
    .unwrap();
    assert!(extension.run(request()).is_err());
    for response in [
        Response {
            edits: vec![edit(0, 0); MAX_EDITS + 1],
            ..Response::default()
        },
        Response {
            logs: vec![String::new(); MAX_LOGS + 1],
            ..Response::default()
        },
        Response {
            logs: vec!["x".repeat(MAX_RESPONSE_BYTES)],
            ..Response::default()
        },
    ] {
        assert!(
            normal(&serde_json::to_string(&response).unwrap())
                .run(request())
                .is_err()
        );
    }
    let response = normal(r#"{"logs":["hello"],"error":"command failed"}"#)
        .run(request())
        .unwrap();
    assert_eq!(response.logs, ["hello"]);
    assert_eq!(response.error.as_deref(), Some("command failed"));
}

#[test]
fn manifest_rejects_unsafe_names_duplicates_versions_and_permissions() {
    let mut cases = Vec::new();
    for id in [
        "",
        "../escape",
        "test/escape",
        "Test",
        "con",
        "test..id",
        "bad:",
        "test.",
    ] {
        let mut m = manifest();
        m.id = id.into();
        cases.push(m);
    }
    for module in [
        "../x.wasm",
        "dir/x.wasm",
        "dir\\x.wasm",
        "C:x.wasm",
        "con.wasm",
        ".hidden.wasm",
        "x.wasm:stream",
        "x.WASM",
        "x..wasm",
    ] {
        let mut m = manifest();
        m.module = module.into();
        cases.push(m);
    }
    for version in ["", "1", "1.0", "01.0.0", "not-a-version"] {
        let mut m = manifest();
        m.version = version.into();
        cases.push(m);
    }
    let mut m = manifest();
    m.api_version += 1;
    cases.push(m);
    let mut m = manifest();
    m.name = " \n ".into();
    cases.push(m);
    let mut m = manifest();
    m.commands.push(m.commands[0].clone());
    cases.push(m);
    let mut m = manifest();
    m.commands[0].id = "../bad".into();
    cases.push(m);
    let mut m = manifest();
    m.commands.clear();
    cases.push(m);
    let mut m = manifest();
    m.permissions = vec![Permission::EditDocument];
    cases.push(m);
    let mut m = manifest();
    m.permissions.push(Permission::ReadDocument);
    cases.push(m);
    for m in cases {
        assert!(package::validate_manifest(&m).is_err(), "{m:?}");
    }
    let value = serde_json::to_value(manifest()).unwrap();
    let mut unknown = value.clone();
    unknown["network"] = true.into();
    assert!(serde_json::from_value::<Manifest>(unknown).is_err());
    let mut unknown = value.clone();
    unknown["commands"][0]["extra"] = true.into();
    assert!(serde_json::from_value::<Manifest>(unknown).is_err());
    let mut unknown = value;
    unknown["permissions"] = serde_json::json!(["network"]);
    assert!(serde_json::from_value::<Manifest>(unknown).is_err());
}

#[test]
fn install_is_non_destructive_copies_only_validated_files_and_never_runs_guest() {
    let temp = TempDir::new().unwrap();
    let wasm = wasm_response(
        b"{}",
        "i32.const 2097152",
        None,
        "(func $start unreachable) (start $start)",
    );
    let source = package(temp.path(), "source", &manifest(), &wasm);
    fs::write(source.join("unrelated-secret"), b"must not copy").unwrap();
    let root = temp.path().join("installed");
    let extension = Extension::install(&source, &root).unwrap();
    assert_eq!(extension.directory(), root.join("test.extension"));
    assert!(!extension.directory().join("unrelated-secret").exists());
    assert_eq!(
        fs::read(extension.directory().join("extension.wasm")).unwrap(),
        wasm
    );
    assert!(extension.run(request()).is_err());
    assert!(Extension::install(&source, &root).is_err());
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    fs::remove_dir_all(extension.directory()).unwrap();
    fs::create_dir(extension.directory()).unwrap(); // Even empty targets must survive.
    assert!(Extension::install(&source, &root).is_err());
    assert!(extension.directory().is_dir());
}

#[test]
fn discover_is_ordered_and_reports_invalid_installed_packages() {
    let temp = TempDir::new().unwrap();
    let wasm = wasm_response(b"{}", "i32.const 2097152", None, "");
    let mut z = manifest();
    z.id = "z.test".into();
    package(temp.path(), "z.test", &z, &wasm);
    let mut a = manifest();
    a.id = "a.test".into();
    package(temp.path(), "a.test", &a, &wasm);
    fs::create_dir(temp.path().join("bad.test")).unwrap();
    fs::create_dir(temp.path().join(".nextcode-install-interrupted")).unwrap();
    fs::write(temp.path().join("state.json"), b"{}").unwrap();
    let found = Extension::discover(temp.path());
    assert_eq!(
        found
            .iter()
            .map(|v| v.0.file_name().unwrap().to_str().unwrap())
            .collect::<Vec<_>>(),
        ["a.test", "bad.test", "z.test"]
    );
    assert!(found[0].1.is_ok());
    assert!(found[1].1.is_err());
    assert!(found[2].1.is_ok());
    assert!(Extension::discover(&temp.path().join("missing")).is_empty());
    let wrong = package(temp.path(), "duplicate.test", &a, &wasm);
    assert!(
        Extension::discover(temp.path())
            .iter()
            .any(|(p, r)| p == &wrong && r.is_err())
    );
}

#[test]
fn invalid_source_leaves_no_installation_and_large_files_are_rejected() {
    let temp = TempDir::new().unwrap();
    let source = package(temp.path(), "source", &manifest(), b"not wasm");
    let root = temp.path().join("installed");
    assert!(Extension::install(&source, &root).is_err());
    assert!(!root.exists());
    fs::write(source.join("extension.json"), vec![b' '; 64 * 1024 + 1]).unwrap();
    assert!(Extension::load(&source).is_err());
    fs::write(
        source.join("extension.json"),
        serde_json::to_vec(&manifest()).unwrap(),
    )
    .unwrap();
    let file = fs::File::create(source.join("extension.wasm")).unwrap();
    file.set_len(16 * 1024 * 1024 + 1).unwrap();
    assert!(Extension::load(&source).is_err());
}

#[cfg(unix)]
#[test]
fn symlink_files_directories_ancestors_and_install_roots_are_rejected() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new().unwrap();
    let wasm = wasm_response(b"{}", "i32.const 2097152", None, "");
    let source = package(temp.path(), "source", &manifest(), &wasm);
    let alias = temp.path().join("alias");
    symlink(&source, &alias).unwrap();
    assert!(Extension::load(&alias).is_err());
    assert!(Extension::load(&alias.join("subdir")).is_err());
    let module = source.join("extension.wasm");
    fs::rename(&module, temp.path().join("outside.wasm")).unwrap();
    symlink(temp.path().join("outside.wasm"), &module).unwrap();
    assert!(Extension::load(&source).is_err());
    assert!(Extension::install(&source, &temp.path().join("install")).is_err());
    fs::remove_file(&module).unwrap();
    fs::write(&module, &wasm).unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let root = temp.path().join("root");
    symlink(&outside, &root).unwrap();
    assert!(Extension::install(&source, &root).is_err());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    fs::remove_file(&root).unwrap();
    fs::create_dir(&root).unwrap();
    symlink(&source, root.join("test.extension")).unwrap();
    assert!(Extension::install(&source, &root).is_err());
    assert!(Extension::discover(&root)[0].1.is_err());
    fs::remove_file(source.join("extension.json")).unwrap();
    fs::write(
        temp.path().join("outside.json"),
        serde_json::to_vec(&manifest()).unwrap(),
    )
    .unwrap();
    symlink(
        temp.path().join("outside.json"),
        source.join("extension.json"),
    )
    .unwrap();
    assert!(Extension::load(&source).is_err());
}

#[test]
fn concurrent_install_has_one_winner_and_cleans_losing_staging() {
    use std::sync::{Arc, Barrier};
    let temp = TempDir::new().unwrap();
    let wasm = wasm_response(b"{}", "i32.const 2097152", None, "");
    let source = package(temp.path(), "source", &manifest(), &wasm);
    let root = temp.path().join("installed");
    let barrier = Arc::new(Barrier::new(2));
    let installers: Vec<_> = (0..2)
        .map(|_| {
            let (source, root, barrier) = (source.clone(), root.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                Extension::install(&source, &root)
            })
        })
        .collect();
    let results: Vec<_> = installers.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    assert_eq!(
        Extension::load(&root.join("test.extension"))
            .unwrap()
            .run(request())
            .unwrap(),
        Response::default()
    );
}

#[test]
fn loaded_module_is_an_immutable_snapshot_of_the_approved_package() {
    let temp = TempDir::new().unwrap();
    let wasm = wasm_response(br#"{"logs":["approved"]}"#, "i32.const 2097152", None, "");
    let source = package(temp.path(), "source", &manifest(), &wasm);
    let extension = Extension::load(&source).unwrap();
    fs::write(source.join("extension.wasm"), b"changed after approval").unwrap();
    assert_eq!(extension.run(request()).unwrap().logs, ["approved"]);
    assert!(Extension::load(&source).is_err());
}

#[test]
fn declared_resource_counts_and_minima_are_checked_without_instantiation() {
    for (source, expected) in [
        ("(module (memory 1025))", "64 MiB"),
        ("(module (memory 1) (memory 1))", "only one linear memory"),
        ("(module (memory 1) (table 16385 funcref))", "16384-element"),
        (
            "(module (memory 1) (table 0 funcref) (table 0 funcref))",
            "at most one table",
        ),
        (
            "(module (table 0 funcref))",
            "must define one linear memory",
        ),
    ] {
        let wasm = wat::parse_str(source).unwrap();
        let error = format!("{:#}", validate_initial_resources(&wasm).unwrap_err());
        assert!(error.contains(expected), "{error}");
    }
    let wasm = wat::parse_str("(module (memory 1024) (table 16384 funcref))").unwrap();
    assert!(validate_initial_resources(&wasm).is_ok());
    let wasm = wasm_response(
        b"{}",
        "i32.const 2097152",
        None,
        "(table 0 funcref) (table 0 funcref)",
    );
    assert!(compile(&wasm, manifest()).is_err());
}

#[test]
fn non_wasm32_memory_features_and_text_modules_are_rejected() {
    for source in ["(module (memory i64 1))", "(module (memory 1) (memory 1))"] {
        let wasm = wat::parse_str(source).unwrap();
        assert!(compile(&wasm, manifest()).is_err());
    }
    // Memory section: one memory, custom-page flag, one initial page, log2 size 0.
    let custom_pages = b"\0asm\x01\0\0\0\x05\x04\x01\x08\x01\x00";
    assert!(validate_initial_resources(custom_pages).is_err());
    assert!(compile(custom_pages, manifest()).is_err());
    assert!(compile(b"(module)", manifest()).is_err());
}

#[test]
fn active_initializer_bounds_are_checked_only_when_the_command_runs() {
    let wasm = wasm_response(
        b"{}",
        "i32.const 2097152",
        None,
        "(data (i32.const 4194304) \"outside initial memory\")",
    );
    let extension = compile(&wasm, manifest()).unwrap();
    let error = format!("{:#}", extension.run(request()).unwrap_err());
    assert!(error.contains("instantiation/start"), "{error}");
    assert!(error.contains("initializers"), "{error}");
}

#[test]
fn extended_constant_expressions_are_rejected_before_approval() {
    // Extended-const initializer trees can recurse on the host stack during
    // compilation/drop, outside guest fuel. The memory ABI does not need it.
    let wasm = wasm_response(
        b"{}",
        "i32.const 2097152",
        None,
        "(global i32 (i32.add (i32.const 0) (i32.const 0)))",
    );
    assert!(compile(&wasm, manifest()).is_err());
}

fn panel_fixture() -> extension_api::PanelUpdate {
    use extension_api::*;
    PanelUpdate {
        refresh_ms: None,
        id: "test.panel".into(),
        state: serde_json::json!({"count": 1}),
        root: Widget::Column {
            id: "root".into(),
            gap: 8,
            children: vec![
                Widget::TextInput {
                    label_inline: false,
                    id: "text".into(),
                    label: "Text".into(),
                    value: "abc".into(),
                    placeholder: "".into(),
                    disabled: false,
                },
                Widget::Checkbox {
                    color: None,
                    id: "check".into(),
                    label: "Check".into(),
                    value: false,
                    disabled: false,
                },
                Widget::Select {
                    label_inline: false,
                    id: "select".into(),
                    label: "Select".into(),
                    value: "a".into(),
                    options: vec![SelectOption {
                        value: "a".into(),
                        label: "A".into(),
                    }],
                    disabled: false,
                },
                Widget::Button {
                    id: "run".into(),
                    text: "Run".into(),
                    disabled: false,
                },
                Widget::Progress {
                    id: "progress".into(),
                    label: "Progress".into(),
                    value: 0.5,
                },
                Widget::Table {
                    id: "table".into(),
                    columns: vec!["Column".into()],
                    rows: vec![vec!["Cell".into()]],
                },
                Widget::Chart {
                    legend: true,
                    id: "chart".into(),
                    title: "Chart".into(),
                    height: 120,
                    chart: ChartSpec::Line {
                        series: vec![ChartSeries {
                            color: None,
                            name: "Series".into(),
                            points: vec![ChartPoint { x: 0.0, y: 1.0 }],
                        }],
                        x_label: "X".into(),
                        y_label: "Y".into(),
                    },
                },
            ],
        },
    }
}

#[test]
fn panel_tree_and_exact_event_envelope_are_validated() {
    use extension_api::*;
    use std::collections::BTreeMap;
    let panel = panel_fixture();
    validate_panel_update(&panel).unwrap();
    let inputs = BTreeMap::from([
        ("text".into(), InputValue::Text("typed".into())),
        ("check".into(), InputValue::Bool(true)),
        ("select".into(), InputValue::Text("a".into())),
    ]);
    let event = UiEvent {
        widget: "run".into(),
        kind: UiEventKind::Click,
    };
    validate_panel_event(&panel, &event, &inputs).unwrap();
    let mut spoof = inputs.clone();
    spoof.insert("foreign".into(), InputValue::Bool(false));
    assert!(validate_panel_event(&panel, &event, &spoof).is_err());
    let mut wrong = inputs.clone();
    wrong.insert("text".into(), InputValue::Bool(true));
    assert!(validate_panel_event(&panel, &event, &wrong).is_err());
    for event in [
        UiEvent {
            widget: "missing".into(),
            kind: UiEventKind::Click,
        },
        UiEvent {
            widget: "text".into(),
            kind: UiEventKind::Click,
        },
        UiEvent {
            widget: "check".into(),
            kind: UiEventKind::Change {
                value: InputValue::Bool(false),
            },
        },
        UiEvent {
            widget: "chart".into(),
            kind: UiEventKind::ChartSelect {
                series: 0,
                index: 1,
            },
        },
        UiEvent {
            widget: "chart".into(),
            kind: UiEventKind::ChartZoom {
                start: 1.0,
                end: 0.0,
            },
        },
    ] {
        assert!(validate_panel_event(&panel, &event, &inputs).is_err());
    }
    let mut disabled = panel.clone();
    if let Widget::Column { children, .. } = &mut disabled.root {
        children[3] = Widget::Button {
            id: "run".into(),
            text: "Run".into(),
            disabled: true,
        };
    }
    assert!(validate_panel_event(&disabled, &event, &inputs).is_err());
    assert_eq!(
        serde_json::from_slice::<PanelUpdate>(&serde_json::to_vec(&panel).unwrap()).unwrap(),
        panel
    );
}

#[test]
fn panel_bounds_and_error_atomicity_apply_without_document_edits() {
    use extension_api::*;
    let mut panel = panel_fixture();
    panel.state = serde_json::json!("x".repeat(16384));
    assert!(validate_panel_update(&panel).is_err());
    panel.state = serde_json::Value::Null;
    for root in [
        Widget::Space {
            id: "space".into(),
            size: 49,
        },
        Widget::Progress {
            id: "p".into(),
            label: "".into(),
            value: f64::NAN,
        },
        Widget::TextInput {
            label_inline: false,
            id: "t".into(),
            label: "".into(),
            value: "x".repeat(4097),
            placeholder: "".into(),
            disabled: false,
        },
        Widget::Label {
            id: "l".into(),
            text: "x".repeat(65537),
        },
        Widget::Table {
            id: "t".into(),
            columns: vec!["a".into()],
            rows: vec![vec![]],
        },
        Widget::Column {
            id: "r".into(),
            gap: 0,
            children: vec![Widget::Space {
                id: "r".into(),
                size: 0,
            }],
        },
        Widget::Chart {
            legend: true,
            id: "c".into(),
            title: "".into(),
            height: 120,
            chart: ChartSpec::Histogram {
                values: vec![1.0; 4097],
                bins: 1,
                x_label: "".into(),
                y_label: "".into(),
            },
        },
    ] {
        panel.root = root;
        assert!(validate_panel_update(&panel).is_err());
    }
    let response = Response {
        panels: vec![panel_fixture()],
        error: Some("failed".into()),
        ..Response::default()
    };
    assert!(validate_response(&response, None, &[]).is_err());
    assert!(
        validate_response(
            &Response {
                panels: vec![panel_fixture()],
                ..Response::default()
            },
            None,
            &[]
        )
        .is_ok()
    );
}

#[test]
fn runtime_panel_scope_uses_immutable_manifest_and_api_two() {
    use extension_api::*;
    let mut policy = manifest();
    policy.panels = vec![PanelContribution {
        id: "test.panel".into(),
        title: "Test".into(),
        placement: PanelPlacement::Dock,
    }];
    let response = Response {
        panels: vec![panel_fixture()],
        ..Response::default()
    };
    let wasm = wasm_response(
        &serde_json::to_vec(&response).unwrap(),
        "i32.const 2097152",
        None,
        "",
    );
    let mut extension = Extension::compile(PathBuf::new(), policy, &wasm).unwrap();
    let mut req = request();
    req.invocation = Invocation::OpenPanel {
        id: "test.panel".into(),
    };
    assert_eq!(extension.run(req.clone()).unwrap().panels, response.panels);
    req.api_version = 1;
    assert!(extension.run(req).is_err());
    extension.manifest.panels.push(PanelContribution {
        id: "spoof".into(),
        title: "Spoof".into(),
        placement: PanelPlacement::Editor,
    });
    let mut req = request();
    req.invocation = Invocation::OpenPanel { id: "spoof".into() };
    assert!(extension.run(req).is_err());
    let mut req = request();
    req.invocation = Invocation::OpenPanel {
        id: "test.panel".into(),
    };
    let empty = normal("{}");
    assert!(empty.run(req).is_err());
}

#[test]
fn charts_and_structural_limits_cover_all_native_shapes() {
    use extension_api::*;
    let mut panel = panel_fixture();
    for chart in [
        ChartSpec::Scatter {
            series: vec![ChartSeries {
                color: None,
                name: "s".into(),
                points: vec![ChartPoint { x: 2.0, y: 3.0 }],
            }],
            x_label: "x".into(),
            y_label: "y".into(),
        },
        ChartSpec::Bar {
            labels: vec!["a".into()],
            series: vec![BarSeries {
                color: None,
                name: "s".into(),
                values: vec![1.0],
            }],
            x_label: "x".into(),
            y_label: "y".into(),
        },
        ChartSpec::Histogram {
            values: vec![0.0, 1.0],
            bins: 64,
            x_label: "x".into(),
            y_label: "y".into(),
        },
    ] {
        panel.root = Widget::Chart {
            legend: true,
            id: "c".into(),
            title: "Chart".into(),
            chart,
            height: 500,
        };
        validate_panel_update(&panel).unwrap();
    }
    for chart in [
        ChartSpec::Line {
            series: vec![],
            x_label: "".into(),
            y_label: "".into(),
        },
        ChartSpec::Scatter {
            series: vec![ChartSeries {
                color: None,
                name: "s".into(),
                points: vec![ChartPoint {
                    x: f64::INFINITY,
                    y: 0.0,
                }],
            }],
            x_label: "".into(),
            y_label: "".into(),
        },
        ChartSpec::Bar {
            labels: vec!["a".into()],
            series: vec![BarSeries {
                color: None,
                name: "s".into(),
                values: vec![1.0, 2.0],
            }],
            x_label: "".into(),
            y_label: "".into(),
        },
        ChartSpec::Histogram {
            values: vec![0.0],
            bins: 0,
            x_label: "".into(),
            y_label: "".into(),
        },
        ChartSpec::Histogram {
            values: vec![1e13],
            bins: 1,
            x_label: "".into(),
            y_label: "".into(),
        },
    ] {
        panel.root = Widget::Chart {
            legend: true,
            id: "c".into(),
            title: "".into(),
            chart,
            height: 120,
        };
        assert!(validate_panel_update(&panel).is_err());
    }
    panel.root = Widget::Column {
        id: "root".into(),
        gap: 0,
        children: (0..128)
            .map(|i| Widget::Space {
                id: format!("s{i}"),
                size: 0,
            })
            .collect(),
    };
    assert!(validate_panel_update(&panel).is_err());
    panel.root = Widget::Label {
        id: "leaf".into(),
        text: "text".into(),
    };
    for depth in 0..8 {
        panel.root = Widget::Scroll {
            id: format!("scroll{depth}"),
            child: Box::new(panel.root),
            height: 32,
        };
    }
    assert!(validate_panel_update(&panel).is_err());
    panel.root = Widget::Select {
        label_inline: false,
        id: "s".into(),
        label: "".into(),
        value: "a".into(),
        options: vec![
            SelectOption {
                value: "a".into(),
                label: "A".into()
            };
            2
        ],
        disabled: false,
    };
    assert!(validate_panel_update(&panel).is_err());
    panel.root = Widget::Scroll {
        id: "scroll".into(),
        height: 600,
        child: Box::new(Widget::Row {
            id: "row".into(),
            gap: 48,
            children: vec![
                Widget::Markdown {
                    id: "markdown".into(),
                    text: "**Generated**".into(),
                },
                Widget::Space {
                    id: "space".into(),
                    size: 48,
                },
            ],
        }),
    };
    validate_panel_update(&panel).unwrap();
}

#[test]
fn panel_runtime_rejects_cross_target_duplicate_and_missing_open_results() {
    use extension_api::*;
    let mut policy = manifest();
    policy.panels = ["test.panel", "other"]
        .into_iter()
        .map(|id| PanelContribution {
            id: id.into(),
            title: id.into(),
            placement: PanelPlacement::Dock,
        })
        .collect();
    for panels in [vec![], vec![panel_fixture(), panel_fixture()]] {
        let response = Response {
            panels,
            ..Response::default()
        };
        let extension = compile(
            &wasm_response(
                &serde_json::to_vec(&response).unwrap(),
                "i32.const 2097152",
                None,
                "",
            ),
            policy.clone(),
        )
        .unwrap();
        let mut req = request();
        req.invocation = Invocation::OpenPanel {
            id: "test.panel".into(),
        };
        assert!(extension.run(req).is_err());
    }
    let response = Response {
        panels: vec![panel_fixture()],
        ..Response::default()
    };
    let extension = compile(
        &wasm_response(
            &serde_json::to_vec(&response).unwrap(),
            "i32.const 2097152",
            None,
            "",
        ),
        policy,
    )
    .unwrap();
    let mut req = request();
    req.invocation = Invocation::UiEvent {
        panel: "other".into(),
        revision: 4,
        state: serde_json::Value::Null,
        inputs: Default::default(),
        event: UiEvent {
            widget: "run".into(),
            kind: UiEventKind::Click,
        },
    };
    assert!(extension.run(req).is_err());
}

#[test]
fn api_two_envelope_preserves_revision_state_and_rejects_old_or_unknown_fields() {
    use extension_api::*;
    let mut req = request();
    req.invocation = Invocation::UiEvent {
        panel: "test.panel".into(),
        revision: u64::MAX,
        state: serde_json::json!({"sequence": 7}),
        inputs: std::collections::BTreeMap::from([(
            "text".into(),
            InputValue::Text("current".into()),
        )]),
        event: UiEvent {
            widget: "text".into(),
            kind: UiEventKind::Submit,
        },
    };
    let encoded = serde_json::to_vec(&req).unwrap();
    assert_eq!(serde_json::from_slice::<Request>(&encoded).unwrap(), req);
    let mut value = serde_json::to_value(&req).unwrap();
    value["command"] = serde_json::json!("test.run");
    assert!(serde_json::from_value::<Request>(value).is_err());
    let old =
        serde_json::json!({"api_version":1,"command":"test.run","document":null,"settings":{}});
    assert!(serde_json::from_value::<Request>(old).is_err());
    let mut value = serde_json::to_value(panel_fixture()).unwrap();
    value["root"]["unexpected"] = true.into();
    assert!(serde_json::from_value::<PanelUpdate>(value).is_err());
    let oversized = Response {
        logs: vec!["x".repeat(MAX_RESPONSE_BYTES)],
        ..Response::default()
    };
    assert!(validate_response(&oversized, None, &[]).is_err());
}

#[test]
fn panel_manifest_accepts_panel_only_and_bounds_unique_contributions() {
    use extension_api::*;
    let mut policy = manifest();
    policy.commands.clear();
    policy.permissions.clear();
    policy.panels = vec![PanelContribution {
        id: "dashboard".into(),
        title: "Dashboard".into(),
        placement: PanelPlacement::Dock,
    }];
    package::validate_manifest(&policy).unwrap();
    let mut duplicate = policy.clone();
    duplicate.panels.push(duplicate.panels[0].clone());
    assert!(package::validate_manifest(&duplicate).is_err());
    policy.panels = (0..9)
        .map(|i| PanelContribution {
            id: format!("panel-{i}"),
            title: "Panel".into(),
            placement: PanelPlacement::Editor,
        })
        .collect();
    assert!(package::validate_manifest(&policy).is_err());
}

#[test]
fn valid_large_select_values_reach_the_guest_but_text_fields_stay_bounded() {
    use extension_api::*;
    use std::collections::BTreeMap;
    let value = "x".repeat(4097);
    let panel = PanelUpdate {
        refresh_ms: None,
        id: "test.panel".into(),
        state: serde_json::Value::Null,
        root: Widget::Column {
            id: "root".into(),
            gap: 0,
            children: vec![
                Widget::Select {
                    label_inline: false,
                    id: "choice".into(),
                    label: "Choice".into(),
                    value: value.clone(),
                    options: vec![SelectOption {
                        value: value.clone(),
                        label: "Large value".into(),
                    }],
                    disabled: false,
                },
                Widget::Button {
                    id: "run".into(),
                    text: "Run".into(),
                    disabled: false,
                },
            ],
        },
    };
    let inputs = BTreeMap::from([("choice".into(), InputValue::Text(value))]);
    let event = UiEvent {
        widget: "run".into(),
        kind: UiEventKind::Click,
    };
    validate_panel_event(&panel, &event, &inputs).unwrap();
    let mut policy = manifest();
    policy.panels.push(PanelContribution {
        id: panel.id.clone(),
        title: "Panel".into(),
        placement: PanelPlacement::Dock,
    });
    // An intentional guest trap distinguishes reaching actual WASM execution
    // from an incorrectly rejected, otherwise valid input envelope.
    let guest = compile(
        &wasm_response(b"{}", "i32.const 2097152", Some("unreachable"), ""),
        policy,
    )
    .unwrap();
    let mut request = request();
    request.invocation = Invocation::UiEvent {
        panel: panel.id,
        revision: 1,
        state: panel.state,
        inputs,
        event,
    };
    let error = guest.run(request).unwrap_err();
    assert!(
        error.downcast_ref::<wasmi::Error>().is_some(),
        "expected a guest trap, got {error:#}"
    );
    let mut text_panel = panel_fixture();
    if let Widget::Column { children, .. } = &mut text_panel.root
        && let Widget::TextInput { value, .. } = &mut children[0]
    {
        *value = "x".repeat(4097);
    }
    assert!(validate_panel_update(&text_panel).is_err());
}

fn process_manifest() -> Manifest {
    let mut m = manifest();
    m.permissions = vec![Permission::RunProcess];
    m.processes = vec![extension_api::DeclaredProcess {
        program: "kx".into(),
    }];
    m.panels = vec![extension_api::PanelContribution {
        id: "overview".into(),
        title: "Overview".into(),
        placement: extension_api::PanelPlacement::Editor,
    }];
    m
}
fn process_request() -> Request {
    Request {
        invocation: Invocation::Refresh {
            panel: "overview".into(),
            revision: 4,
            state: serde_json::json!({}),
            inputs: Default::default(),
        },
        document: None,
        ..request()
    }
}
#[test]
fn process_declarations_require_permission_bare_unique_names_and_api_three() {
    let original = process_manifest();
    package::validate_manifest(&original).unwrap();
    for name in ["", "/bin/kx", "../kx", "a/b", "a\\b", "kx;sh", "con", ".kx"] {
        let mut m = original.clone();
        m.processes[0].program = name.into();
        assert!(package::validate_manifest(&m).is_err(), "{name}");
    }
    let mut m = original.clone();
    m.processes.push(m.processes[0].clone());
    assert!(package::validate_manifest(&m).is_err());
    let mut m = original.clone();
    m.permissions.clear();
    assert!(package::validate_manifest(&m).is_err());
    let mut m = original.clone();
    m.processes.clear();
    assert!(package::validate_manifest(&m).is_err());
    let mut m = original;
    m.api_version = 2;
    assert!(package::validate_manifest(&m).is_err());
}
#[test]
fn process_effects_obey_immutable_policy_scope_and_atomic_validation() {
    let wasm = wasm_response(b"{}", "i32.const 2097152", None, "");
    let mut extension = compile(&wasm, process_manifest()).unwrap();
    let req = process_request();
    let valid = serde_json::json!({"processes":[{"id":"status","program":"kx","args":["nextcode","panel"]}]});
    let response: Response = serde_json::from_value(valid.clone()).unwrap();
    extension.validate_reply(&req, &response).unwrap();
    assert_eq!(response.processes[0].timeout_ms, 30_000);
    for bad in [
        serde_json::json!({"processes":[{"id":"s","program":"sh"}]}),
        serde_json::json!({"processes":[{"id":"s","program":"kx","timeout_ms":120001}]}),
        serde_json::json!({"processes":[{"id":"s","program":"kx","args":["bad\u{0}arg"]}]}),
        serde_json::json!({"processes":[{"id":"s","program":"kx"},{"id":"s","program":"kx"}]}),
        serde_json::json!({"error":"failed","processes":[{"id":"s","program":"kx"}]}),
    ] {
        let response: Response = serde_json::from_value(bad).unwrap();
        assert!(extension.validate_reply(&req, &response).is_err());
    }
    assert!(extension.validate_reply(&request(), &response).is_err());
    extension
        .manifest
        .processes
        .push(extension_api::DeclaredProcess {
            program: "sh".into(),
        });
    let undeclared: Response =
        serde_json::from_value(serde_json::json!({"processes":[{"id":"s","program":"sh"}]}))
            .unwrap();
    assert!(extension.validate_reply(&req, &undeclared).is_err());
    let other = Response {
        panels: vec![panel_fixture()],
        ..Default::default()
    };
    assert!(extension.validate_reply(&req, &other).is_err());
}
#[test]
fn api_two_packages_still_run_but_cannot_acquire_processes_or_timers() {
    let mut m = manifest();
    m.api_version = 2;
    let wasm = wasm_response(b"{}", "i32.const 2097152", None, "");
    let extension = compile(&wasm, m).unwrap();
    let mut req = request();
    req.api_version = 2;
    extension.run(req.clone()).unwrap();
    let effects: Response =
        serde_json::from_value(serde_json::json!({"processes":[{"id":"s","program":"kx"}]}))
            .unwrap();
    assert!(extension.validate_reply(&req, &effects).is_err());
    let mut panel = panel_fixture();
    panel.refresh_ms = Some(999);
    assert!(validate_panel_update(&panel).is_err());
    panel.refresh_ms = Some(1000);
    validate_panel_update(&panel).unwrap();
}

#[test]
fn compact_widgets_accept_legacy_defaults_and_validate_shared_colors() {
    use extension_api::*;
    let fixture = panel_fixture();
    let mut legacy = serde_json::to_value(&fixture).unwrap();
    fn strip(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(fields) => {
                for key in ["color", "label_inline", "legend"] {
                    fields.remove(key);
                }
                for value in fields.values_mut() {
                    strip(value);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    strip(value);
                }
            }
            _ => {}
        }
    }
    strip(&mut legacy);
    let old: PanelUpdate = serde_json::from_value(legacy).unwrap();
    assert_eq!(old, fixture);
    validate_panel_update(&old).unwrap();
    let mut compact = serde_json::to_value(&fixture).unwrap();
    let children = compact["root"]["children"].as_array_mut().unwrap();
    children[0]["label"] = serde_json::json!("");
    children[0]["label_inline"] = serde_json::json!(true);
    children[1]["color"] = serde_json::json!(11);
    children[2]["label_inline"] = serde_json::json!(true);
    children[6]["legend"] = serde_json::json!(false);
    children[6]["chart"]["series"][0]["color"] = serde_json::json!(11);
    let panel: PanelUpdate = serde_json::from_value(compact.clone()).unwrap();
    validate_panel_update(&panel).unwrap();
    assert!(
        matches!(&panel.root, Widget::Column { children, .. } if matches!(children[6], Widget::Chart { legend: false, .. }))
    );
    for pointer in [
        "/root/children/1/color",
        "/root/children/6/chart/series/0/color",
    ] {
        let mut invalid = compact.clone();
        *invalid.pointer_mut(pointer).unwrap() = serde_json::json!(12);
        let invalid: PanelUpdate = serde_json::from_value(invalid).unwrap();
        assert!(
            validate_panel_update(&invalid)
                .unwrap_err()
                .to_string()
                .contains("color")
        );
    }
}
