//! Finding and starting debug adapters where the project lives, and the
//! launch defaults each adapter needs.
//!
//! - Python: debugpy's adapter (`python -m debugpy.adapter`) from the
//!   virtualenv of the project folder holding the launch `cwd` when it has
//!   debugpy, else an ephemeral uv environment (`uv run --no-project
//!   --with debugpy`), else a `python3` with debugpy on `PATH`. The
//!   program runs with that folder's interpreter (`python` in the launch
//!   arguments), not the adapter's.
//! - LLDB: CodeLLDB (`codelldb`, on `PATH` or downloaded into next_code's
//!   data directory where the project lives), else `lldb-dap`.
//! - Custom: the request's command.
//!
//! `"debug": {"adapters": {"python" | "lldb": {"command", "args"}}}` in
//! settings.json replaces the automatic choice. Everything here blocks
//! (it may run programs through the agent): call it off the UI thread.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Value, json};

use super::{DebugAdapterKind, DebugRequest, DebugRequestKind};
use crate::{project::Project, settings::DebugSettings};

/// The CodeLLDB release downloaded on request.
pub const CODELLDB_VERSION: &str = "v1.12.3";

/// The SHA-256 of each [`CODELLDB_VERSION`] release asset, computed from
/// the files at
/// `https://github.com/vadimcn/codelldb/releases/download/v1.12.3/<asset>`;
/// a download is verified against it before anything is unpacked (and
/// deleted when it does not match).
fn codelldb_sha256(asset: &str) -> Option<&'static str> {
    Some(match asset {
        "codelldb-win32-x64.vsix" => {
            "a916e509308dac817732f63ca604a8b93ed29cd16f38a2fa9f0b64ed58e8f51a"
        }
        "codelldb-linux-x64.vsix" => {
            "1cd7f386598022b51a5b93b9ffa23e812b23f519cfe1833384ec4bef4bfd1be1"
        }
        "codelldb-linux-arm64.vsix" => {
            "0887f67d440554617894266f80706b700907c36b95e6e49d23b95a0e05318101"
        }
        "codelldb-linux-armhf.vsix" => {
            "20294f0ea876a73c662bb09b1814551a859dc77156564b62c75acf1088ecaf30"
        }
        "codelldb-darwin-x64.vsix" => {
            "e25cc716b94c62c07fec268ff2785d2b797245b160502baef8b9c970a0c4d8e8"
        }
        "codelldb-darwin-arm64.vsix" => {
            "2f114a990e1b368dd1dbd33c80c0e719767af2d228391ec0df0571c957f9ac91"
        }
        _ => return None,
    })
}

/// The first SHA-256 hex digest in a hashing program's output
/// (`sha256sum`, `certutil -hashfile`, which may space the bytes out, or
/// `Get-FileHash`), lower case.
fn sha256_in(output: &str) -> Option<String> {
    let is_digest = |token: &str| token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit());
    output
        .lines()
        .flat_map(|line| {
            std::iter::once(line.split_whitespace().collect::<String>())
                .chain(line.split_whitespace().map(str::to_owned))
        })
        .find(|token| is_digest(token))
        .map(|token| token.to_lowercase())
}

/// How long probing for debugpy may take (a cold interpreter start).
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Downloading and unpacking CodeLLDB (~55 MB).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How to start an adapter, and what to send it.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub program: String,
    pub args: Vec<String>,
    /// `adapterID` of `initialize`.
    pub adapter_id: String,
    /// The `launch`/`attach` arguments with the adapter's defaults.
    pub configuration: Value,
    /// Shown in the Debug Console when the session starts.
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareError {
    /// No adapter was found; `download` offers to fetch CodeLLDB.
    Missing {
        message: String,
        download: bool,
    },
    Failed(String),
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { message, .. } | Self::Failed(message) => f.write_str(message),
        }
    }
}

impl From<std::io::Error> for PrepareError {
    fn from(err: std::io::Error) -> Self {
        Self::Failed(err.to_string())
    }
}

/// Finds the adapter for `request` and fills in its launch defaults.
pub fn prepare(
    project: &Project,
    request: &DebugRequest,
    settings: &DebugSettings,
) -> Result<Prepared, PrepareError> {
    let mut configuration = match &request.configuration {
        Value::Object(_) => request.configuration.clone(),
        Value::Null => json!({}),
        _ => {
            return Err(PrepareError::Failed(
                "the debug configuration must be a JSON object".into(),
            ));
        }
    };
    let root = project.root().to_string_lossy().into_owned();
    if configuration.get("cwd").is_none() {
        configuration["cwd"] = root.into();
    }
    // The folder the program runs in: its virtualenv and tools.
    let folder = project
        .root_for(&project.path(configuration["cwd"].as_str().unwrap_or_default()))
        .path
        .clone();
    let request_kind = match request.request {
        DebugRequestKind::Launch => "launch",
        DebugRequestKind::Attach => "attach",
    };
    configuration["request"] = request_kind.into();
    if configuration.get("name").is_none() {
        configuration["name"] = request.name.clone().into();
    }
    match &request.adapter {
        DebugAdapterKind::Python => {
            let (program, args, description) =
                python_adapter(project, &folder, settings.adapters.get("python"))?;
            configuration["type"] = "debugpy".into();
            set_default(&mut configuration, "console", "internalConsole".into());
            set_default(&mut configuration, "justMyCode", true.into());
            if request.request == DebugRequestKind::Launch
                && configuration.get("python").is_none()
                && let Some(python) = program_interpreter(project, &folder)?
            {
                configuration["python"] = python.into();
            }
            Ok(Prepared {
                program,
                args,
                adapter_id: "debugpy".into(),
                configuration,
                description,
            })
        }
        DebugAdapterKind::Lldb => {
            let (program, args, flavor) = lldb_adapter(project, settings.adapters.get("lldb"))?;
            configuration["type"] = "lldb".into();
            match flavor {
                LldbFlavor::CodeLldb => {
                    // Program output in the Debug Console, not a terminal.
                    set_default(&mut configuration, "terminal", "console".into());
                    set_default(&mut configuration, "sourceLanguages", json!(["rust"]));
                }
                LldbFlavor::LldbDap => {
                    // lldb-dap wants `env` as `["K=V"]`.
                    if let Some(env) = configuration.get("env").and_then(Value::as_object) {
                        let pairs: Vec<Value> = env
                            .iter()
                            .map(|(key, value)| {
                                format!("{key}={}", value.as_str().unwrap_or_default()).into()
                            })
                            .collect();
                        configuration["env"] = pairs.into();
                    }
                }
            }
            let description = match flavor {
                LldbFlavor::CodeLldb => format!("CodeLLDB ({program})"),
                LldbFlavor::LldbDap => format!("lldb-dap ({program})"),
            };
            Ok(Prepared {
                program,
                args,
                adapter_id: "lldb".into(),
                configuration,
                description,
            })
        }
        DebugAdapterKind::Custom { command, args } => {
            let program = project
                .find_executable_in(command, &folder)?
                .map(|path| path.to_string_lossy().into_owned())
                .ok_or_else(|| PrepareError::Missing {
                    message: format!("The debug adapter `{command}` was not found"),
                    download: false,
                })?;
            Ok(Prepared {
                description: format!("{program} {}", args.join(" ")),
                program,
                args: args.clone(),
                adapter_id: "custom".into(),
                configuration,
            })
        }
    }
}

fn set_default(configuration: &mut Value, key: &str, value: Value) {
    if configuration.get(key).is_none() {
        configuration[key] = value;
    }
}

fn python_adapter(
    project: &Project,
    folder: &Path,
    setting: Option<&crate::settings::AdapterCommandSetting>,
) -> Result<(String, Vec<String>, String), PrepareError> {
    if let Some(setting) = setting {
        return Ok((
            setting.command.clone(),
            setting.args.clone(),
            format!("{} (settings)", setting.command),
        ));
    }
    let adapter_args = || ["-m", "debugpy.adapter"].map(str::to_owned).to_vec();
    if let Some(python) = project.python_interpreter_in(folder)? {
        let python = python.to_string_lossy().into_owned();
        if has_debugpy(project, &python) {
            return Ok((
                python.clone(),
                adapter_args(),
                format!("debugpy ({python})"),
            ));
        }
    }
    if let Some(uv) = project.find_executable_in("uv", folder)? {
        let args = [
            "run",
            "--quiet",
            "--no-project",
            "--with",
            "debugpy",
            "python",
            "-m",
            "debugpy.adapter",
        ]
        .map(str::to_owned)
        .to_vec();
        return Ok((
            uv.to_string_lossy().into_owned(),
            args,
            "debugpy (uv run --with debugpy)".into(),
        ));
    }
    for name in python_names(project) {
        if let Some(python) = project.find_executable_in(name, folder)? {
            let python = python.to_string_lossy().into_owned();
            if has_debugpy(project, &python) {
                return Ok((
                    python.clone(),
                    adapter_args(),
                    format!("debugpy ({python})"),
                ));
            }
        }
    }
    Err(PrepareError::Missing {
        message: "Python debugging needs debugpy: install uv (https://docs.astral.sh/uv/), \
                  or install debugpy into the project's environment (`uv add --dev debugpy` \
                  or `pip install debugpy`)"
            .into(),
        download: false,
    })
}

/// Interpreter names to try on `PATH`; on Windows `python3` is often the
/// Microsoft Store stub.
fn python_names(project: &Project) -> [&'static str; 2] {
    if cfg!(windows) && !project.is_remote() {
        ["python", "python3"]
    } else {
        ["python3", "python"]
    }
}

fn has_debugpy(project: &Project, python: &str) -> bool {
    let args = ["-c".to_owned(), "import debugpy".to_owned()];
    project
        .run_program(python, &args, &[], None, PROBE_TIMEOUT)
        .is_ok_and(|output| output.code == Some(0))
}

/// The interpreter the program runs with: the virtualenv of its folder,
/// else Python on `PATH` (`None`: debugpy uses its own).
fn program_interpreter(project: &Project, folder: &Path) -> Result<Option<String>, PrepareError> {
    if let Some(python) = project.python_interpreter_in(folder)? {
        return Ok(Some(python.to_string_lossy().into_owned()));
    }
    for name in python_names(project) {
        if let Some(python) = project.find_executable_in(name, folder)? {
            return Ok(Some(python.to_string_lossy().into_owned()));
        }
    }
    Ok(None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LldbFlavor {
    CodeLldb,
    LldbDap,
}

fn lldb_adapter(
    project: &Project,
    setting: Option<&crate::settings::AdapterCommandSetting>,
) -> Result<(String, Vec<String>, LldbFlavor), PrepareError> {
    if let Some(setting) = setting {
        let flavor = if setting.command.contains("codelldb") {
            LldbFlavor::CodeLldb
        } else {
            LldbFlavor::LldbDap
        };
        return Ok((setting.command.clone(), setting.args.clone(), flavor));
    }
    if let Some(codelldb) = project.find_executable("codelldb")? {
        let codelldb = codelldb.to_string_lossy().into_owned();
        return Ok((codelldb, Vec::new(), LldbFlavor::CodeLldb));
    }
    if let Some(codelldb) = downloaded_codelldb(project)? {
        return Ok((codelldb, Vec::new(), LldbFlavor::CodeLldb));
    }
    for name in ["lldb-dap", "lldb-vscode"] {
        if let Some(lldb_dap) = project.find_executable(name)? {
            let lldb_dap = lldb_dap.to_string_lossy().into_owned();
            return Ok((lldb_dap, Vec::new(), LldbFlavor::LldbDap));
        }
    }
    Err(PrepareError::Missing {
        message: format!(
            "No LLDB debug adapter found {}: download CodeLLDB {CODELLDB_VERSION}, put \
             `codelldb` or `lldb-dap` on PATH, or set debug.adapters.lldb in settings.json",
            where_label(project)
        ),
        download: true,
    })
}

fn where_label(project: &Project) -> &'static str {
    if project.is_remote() {
        "where the project lives"
    } else {
        "on this machine"
    }
}

/// next_code's adapter directory where the project lives.
fn adapters_dir(project: &Project) -> Result<PathBuf, PrepareError> {
    if !project.is_remote() {
        let data = dirs::data_local_dir()
            .ok_or_else(|| PrepareError::Failed("no local data directory".into()))?;
        return Ok(data.join("next_code").join("adapters"));
    }
    let output = project.run_program(
        "/bin/sh",
        &[
            "-c".into(),
            "printf %s \"${XDG_DATA_HOME:-$HOME/.local/share}\"".into(),
        ],
        &[],
        None,
        PROBE_TIMEOUT,
    )?;
    let data = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.code != Some(0) || !data.starts_with('/') {
        return Err(PrepareError::Failed(
            "cannot find the data directory where the project lives".into(),
        ));
    }
    Ok(PathBuf::from(format!("{data}/next_code/adapters")))
}

/// Where CodeLLDB is (or would be) unpacked.
fn codelldb_dir(project: &Project) -> Result<PathBuf, PrepareError> {
    let adapters = adapters_dir(project)?;
    Ok(project.join(&adapters, &format!("codelldb-{CODELLDB_VERSION}")))
}

fn codelldb_executable(project: &Project, dir: &Path) -> PathBuf {
    let name = if cfg!(windows) && !project.is_remote() {
        "extension/adapter/codelldb.exe"
    } else {
        "extension/adapter/codelldb"
    };
    project.join(dir, name)
}

fn downloaded_codelldb(project: &Project) -> Result<Option<String>, PrepareError> {
    let executable = codelldb_executable(project, &codelldb_dir(project)?);
    Ok(
        matches!(project.kind(&executable)?, Some(worktree::FileKind::File))
            .then(|| executable.to_string_lossy().into_owned()),
    )
}

/// The CodeLLDB release asset for the machine where the project lives.
fn codelldb_asset(project: &Project) -> Result<&'static str, PrepareError> {
    if !project.is_remote() {
        return match (std::env::consts::OS, std::env::consts::ARCH) {
            ("windows", "x86_64") => Ok("codelldb-win32-x64.vsix"),
            ("linux", "x86_64") => Ok("codelldb-linux-x64.vsix"),
            ("linux", "aarch64") => Ok("codelldb-linux-arm64.vsix"),
            ("macos", "x86_64") => Ok("codelldb-darwin-x64.vsix"),
            ("macos", "aarch64") => Ok("codelldb-darwin-arm64.vsix"),
            (os, arch) => Err(PrepareError::Failed(format!(
                "CodeLLDB has no release for {os}/{arch}"
            ))),
        };
    }
    let output = project.run_program("uname", &["-m".into()], &[], None, PROBE_TIMEOUT)?;
    match String::from_utf8_lossy(&output.stdout).trim() {
        "x86_64" | "amd64" => Ok("codelldb-linux-x64.vsix"),
        "aarch64" | "arm64" => Ok("codelldb-linux-arm64.vsix"),
        "armv7l" => Ok("codelldb-linux-armhf.vsix"),
        arch => Err(PrepareError::Failed(format!(
            "CodeLLDB has no Linux release for {arch}"
        ))),
    }
}

/// Downloads a CodeLLDB `.vsix` (a zip), checks its SHA-256 against the
/// expected one (`$3`) and unpacks it, with whatever the machine has.
const UNPACK_SCRIPT: &str = r#"set -e
dir="$1"; url="$2"; sha="$3"
mkdir -p "$dir"; cd "$dir"
if command -v curl >/dev/null 2>&1; then curl -fsSL -o codelldb.vsix "$url"
elif command -v wget >/dev/null 2>&1; then wget -qO codelldb.vsix "$url"
else echo "downloading needs curl or wget" >&2; exit 3; fi
if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum codelldb.vsix | cut -d' ' -f1) || actual=
elif command -v shasum >/dev/null 2>&1; then actual=$(shasum -a 256 codelldb.vsix | cut -d' ' -f1) || actual=
elif command -v openssl >/dev/null 2>&1; then actual=$(openssl dgst -sha256 codelldb.vsix | sed 's/^.*= *//') || actual=
else rm -f codelldb.vsix; echo "verifying the download needs sha256sum, shasum or openssl" >&2; exit 3; fi
if [ "$actual" != "$sha" ]; then rm -f codelldb.vsix; echo "codelldb.vsix: SHA-256 '$actual' is not the expected $sha; deleted" >&2; exit 4; fi
if command -v unzip >/dev/null 2>&1; then unzip -qo codelldb.vsix 'extension/*'
elif command -v bsdtar >/dev/null 2>&1; then bsdtar -xf codelldb.vsix extension
elif command -v python3 >/dev/null 2>&1; then python3 -m zipfile -e codelldb.vsix .
else echo "unpacking needs unzip, bsdtar or python3" >&2; exit 3; fi
chmod +x extension/adapter/codelldb extension/lldb/bin/* 2>/dev/null || true
rm -f codelldb.vsix"#;

/// Downloads CodeLLDB for the machine where the project lives into
/// next_code's data directory there; returns the adapter's path.
pub fn download_codelldb(project: &Project) -> Result<String, String> {
    let fail = |err: PrepareError| err.to_string();
    let asset = codelldb_asset(project).map_err(fail)?;
    let sha256 = codelldb_sha256(asset)
        .ok_or_else(|| format!("no known SHA-256 for {asset}: it cannot be verified"))?;
    let dir = codelldb_dir(project).map_err(fail)?;
    let url =
        format!("https://github.com/vadimcn/codelldb/releases/download/{CODELLDB_VERSION}/{asset}");
    let dir_text = dir.to_string_lossy().into_owned();
    let output = if cfg!(windows) && !project.is_remote() {
        std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        let archive = dir.join("codelldb.vsix").to_string_lossy().into_owned();
        // curl.exe and bsdtar (tar.exe, which reads zips) ship with Windows 10+.
        let download = project
            .run_program(
                "curl.exe",
                &["-fsSL".into(), "-o".into(), archive.clone(), url.clone()],
                &[],
                None,
                DOWNLOAD_TIMEOUT,
            )
            .map_err(|err| format!("curl.exe: {err}"))?;
        if download.code != Some(0) {
            download
        } else {
            if let Err(message) = verify_sha256_windows(project, &archive, sha256) {
                std::fs::remove_file(&archive).ok();
                return Err(format!("Downloading {url} failed: {message}; deleted"));
            }
            let unpacked = project
                .run_program(
                    "tar.exe",
                    &[
                        "-xf".into(),
                        archive.clone(),
                        "-C".into(),
                        dir_text.clone(),
                        "extension".into(),
                    ],
                    &[],
                    None,
                    DOWNLOAD_TIMEOUT,
                )
                .map_err(|err| format!("tar.exe: {err}"))?;
            std::fs::remove_file(&archive).ok();
            unpacked
        }
    } else {
        project
            .run_program(
                "/bin/sh",
                &[
                    "-c".into(),
                    UNPACK_SCRIPT.into(),
                    "sh".into(),
                    dir_text,
                    url.clone(),
                    sha256.into(),
                ],
                &[],
                None,
                DOWNLOAD_TIMEOUT,
            )
            .map_err(|err| err.to_string())?
    };
    if output.code != Some(0) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Downloading {url} failed: {}",
            stderr.trim().lines().last().unwrap_or("unknown error")
        ));
    }
    downloaded_codelldb(project)
        .map_err(fail)?
        .ok_or_else(|| format!("{asset} did not contain the CodeLLDB adapter"))
}

/// Checks `archive`'s SHA-256 (on this Windows machine) with `certutil`,
/// which ships with Windows, else PowerShell's `Get-FileHash`.
fn verify_sha256_windows(project: &Project, archive: &str, expected: &str) -> Result<(), String> {
    let attempts: [(&str, Vec<String>); 2] = [
        (
            "certutil.exe",
            vec!["-hashfile".into(), archive.into(), "SHA256".into()],
        ),
        (
            "powershell.exe",
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                format!(
                    "(Get-FileHash -Algorithm SHA256 -LiteralPath '{}').Hash",
                    archive.replace('\'', "''")
                ),
            ],
        ),
    ];
    let mut problems = Vec::new();
    for (program, args) in &attempts {
        match project.run_program(program, args, &[], None, PROBE_TIMEOUT) {
            Ok(output) if output.code == Some(0) => {
                let Some(actual) = sha256_in(&String::from_utf8_lossy(&output.stdout)) else {
                    problems.push(format!("{program} printed no SHA-256"));
                    continue;
                };
                if actual == expected {
                    return Ok(());
                }
                return Err(format!(
                    "{archive}: SHA-256 {actual} is not the expected {expected} for CodeLLDB \
                     {CODELLDB_VERSION}"
                ));
            }
            Ok(output) => problems.push(format!(
                "{program}: {}",
                String::from_utf8_lossy(&output.stderr)
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or("failed")
            )),
            Err(err) => problems.push(format!("{program}: {err}")),
        }
    }
    Err(format!(
        "cannot verify the download ({})",
        problems.join("; ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_release_asset_has_a_known_hash() {
        for asset in [
            "codelldb-win32-x64.vsix",
            "codelldb-linux-x64.vsix",
            "codelldb-linux-arm64.vsix",
            "codelldb-linux-armhf.vsix",
            "codelldb-darwin-x64.vsix",
            "codelldb-darwin-arm64.vsix",
        ] {
            let hash = codelldb_sha256(asset).unwrap_or_else(|| panic!("{asset}"));
            assert_eq!(hash.len(), 64);
            assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
        }
        assert_eq!(codelldb_sha256("codelldb-haiku-x64.vsix"), None);
    }

    #[test]
    fn digests_are_found_in_hashing_programs_output() {
        let digest = "A916E509308DAC817732F63CA604A8B93ED29CD16F38A2FA9F0B64ED58E8F51A";
        let lower = digest.to_lowercase();
        assert_eq!(
            sha256_in(&format!("{lower}  codelldb.vsix\n")),
            Some(lower.clone())
        );
        assert_eq!(
            sha256_in(&format!(
                "SHA256 hash of C:\\x\\codelldb.vsix:\r\n{digest}\r\nCertUtil: -hashfile command completed successfully.\r\n"
            )),
            Some(lower.clone())
        );
        // Older certutil versions space the bytes out.
        let spaced: String = digest
            .as_bytes()
            .chunks(2)
            .map(|pair| String::from_utf8_lossy(pair).into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            sha256_in(&format!("hash:\n{spaced}\n")),
            Some(lower.clone())
        );
        assert_eq!(sha256_in(&format!("{digest}\r\n")), Some(lower));
        assert_eq!(
            sha256_in("CertUtil: The system cannot find the file specified."),
            None
        );
        assert_eq!(sha256_in(""), None);
    }
}
