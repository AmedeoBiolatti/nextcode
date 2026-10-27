//! Compiler invocations run on the same host as the project. Assembly is
//! captured directly, or in Cargo's target directory, never over a source file.

use std::{
    io,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;
use syntax::Language;

use crate::project::{CancelToken, Project};

#[derive(Clone, Debug)]
pub struct Compiler {
    pub label: String,
    pub program: String,
    pub cwd: PathBuf,
    pub args: Vec<String>,
    pub cargo_output: Option<PathBuf>,
    pub rust: bool,
}

pub struct Output {
    pub assembly: String,
    pub diagnostics: String,
    pub command: String,
}

pub fn supported(path: &Path) -> bool {
    matches!(
        Language::from_path(path),
        Some(Language::C | Language::Cpp | Language::Rust)
    )
}

fn failure(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn run(
    project: &Project,
    compiler: &Compiler,
    args: &[String],
    cancel: &CancelToken,
) -> io::Result<remote::RunOutput> {
    project.run_program_cancellable(
        &compiler.cwd,
        &compiler.program,
        args,
        &[],
        None,
        crate::run_config::BUILD_TIMEOUT,
        Some(cancel),
    )
}

pub fn compile(
    project: &Project,
    compiler: &Compiler,
    optimization: u8,
    extra: &str,
    cancel: &CancelToken,
) -> io::Result<Output> {
    let mut args = compiler.args.clone();
    let mut output_file = None;
    if let Some(dir) = &compiler.cargo_output {
        project.create_dir_all(dir)?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        output_file =
            Some(project.join(dir, &format!("assembly-{}-{nonce}.s", std::process::id())));
        if optimization > 0 {
            args.push("--release".into());
        }
        args.push("--".into());
    }
    args.extend(run::shell::split(extra));
    if compiler.rust {
        args.extend([
            "-C".into(),
            format!("opt-level={optimization}"),
            "-C".into(),
            "debuginfo=0".into(),
            "-C".into(),
            "codegen-units=1".into(),
            "-C".into(),
            "lto=off".into(),
            format!(
                "--emit=asm={}",
                output_file
                    .as_ref()
                    .map_or_else(|| "-".into(), |p| p.to_string_lossy().into_owned())
            ),
        ]);
    } else {
        args.extend([
            "-S".into(),
            format!("-O{optimization}"),
            "-g0".into(),
            "-fno-lto".into(),
            "-o".into(),
            "-".into(),
        ]);
    }
    let command = std::iter::once(&compiler.program)
        .chain(args.iter())
        .map(|arg| run::shell::quote(arg, run::shell::Shell::Posix))
        .collect::<Vec<_>>()
        .join(" ");
    let result = (|| {
        let output = run(project, compiler, &args, cancel)?;
        let diagnostics = String::from_utf8_lossy(&output.stderr).into_owned();
        if output.code != Some(0) {
            return Err(failure(format!(
                "{command}\n\n{diagnostics}{}",
                String::from_utf8_lossy(&output.stdout)
            )));
        }
        let assembly = match &output_file {
            Some(path) => project.read_text(path)?,
            None => String::from_utf8(output.stdout).map_err(|err| failure(err.to_string()))?,
        };
        if assembly.trim().is_empty() {
            return Err(failure(format!(
                "The compiler produced no assembly. Public functions may have been optimized away.\n\n{diagnostics}"
            )));
        }
        Ok(Output {
            assembly,
            diagnostics,
            command,
        })
    })();
    if let Some(path) = output_file {
        let _ = project.remove(&path, false);
    }
    result
}

pub fn discover(project: &Project, path: &Path, cancel: &CancelToken) -> io::Result<Vec<Compiler>> {
    match Language::from_path(path) {
        Some(Language::Rust) => rust_compilers(project, path, cancel),
        Some(Language::C | Language::Cpp) => c_compilers(project, path, cancel),
        _ => Err(failure(
            "Open a C, C++ or Rust source file to inspect its assembly.",
        )),
    }
}

fn ancestors<'a>(project: &'a Project, path: &'a Path) -> impl Iterator<Item = &'a Path> {
    let root = &project.root_for(path).path;
    path.parent()
        .into_iter()
        .flat_map(Path::ancestors)
        .take_while(move |dir| dir.starts_with(root))
}

fn rust_compilers(
    project: &Project,
    path: &Path,
    cancel: &CancelToken,
) -> io::Result<Vec<Compiler>> {
    let manifest = ancestors(project, path)
        .map(|dir| project.join(dir, "Cargo.toml"))
        .find(|file| project.kind(file).ok().flatten().is_some());
    let Some(manifest) = manifest else {
        return Ok(vec![Compiler {
            label: "rustc (standalone library)".into(),
            program: "rustc".into(),
            cwd: path.parent().unwrap_or(project.root()).to_path_buf(),
            args: vec![
                path.to_string_lossy().into_owned(),
                "--crate-type=lib".into(),
                "--edition=2024".into(),
            ],
            cargo_output: None,
            rust: true,
        }]);
    };
    let mut compiler = Compiler {
        label: "Cargo".into(),
        program: "cargo".into(),
        cwd: manifest.parent().unwrap().to_path_buf(),
        args: Vec::new(),
        cargo_output: None,
        rust: true,
    };
    let args = vec![
        "metadata".into(),
        "--no-deps".into(),
        "--format-version=1".into(),
        "--manifest-path".into(),
        manifest.to_string_lossy().into_owned(),
    ];
    let output = run(project, &compiler, &args, cancel)?;
    if output.code != Some(0) {
        return Err(failure(String::from_utf8_lossy(&output.stderr)));
    }
    let json = String::from_utf8_lossy(&output.stdout);
    let metadata: serde_json::Value =
        serde_json::from_str(&json).map_err(|err| failure(err.to_string()))?;
    let packages = run::cargo::parse_metadata(&json).map_err(failure)?;
    let package = packages
        .iter()
        .filter(|package| path.starts_with(&package.dir))
        .max_by_key(|package| package.dir.len())
        .ok_or_else(|| failure("No Cargo package contains this source file."))?;
    let target_dir = metadata["target_directory"]
        .as_str()
        .ok_or_else(|| failure("Cargo metadata has no target directory."))?;
    compiler.cargo_output = Some(project.join(Path::new(target_dir), "next_code-assembly"));
    compiler.args = vec![
        "rustc".into(),
        "--quiet".into(),
        "--manifest-path".into(),
        manifest.to_string_lossy().into_owned(),
        "--package".into(),
        package.name.clone(),
    ];
    let mut targets = package.targets.clone();
    // Exact crate roots first, then the closest source directory (e.g. examples/),
    // with a library preferred for modules shared by lib and bin in src/.
    targets.sort_by_key(|target| {
        (
            std::cmp::Reverse(path == Path::new(&target.src_path)),
            std::cmp::Reverse(
                Path::new(&target.src_path)
                    .parent()
                    .filter(|dir| path.starts_with(dir))
                    .map_or(0, |dir| dir.as_os_str().len()),
            ),
            !matches!(target.target, run::config::CargoTarget::Lib),
        )
    });
    let compilers = targets
        .iter()
        .map(|target| {
            let mut compiler = compiler.clone();
            compiler.label = format!("{} · {}", package.name, target.target.label());
            compiler.args.extend(target.target.args());
            compiler
        })
        .collect::<Vec<_>>();
    if compilers.is_empty() {
        return Err(failure("This Cargo package has no compilable targets."));
    }
    Ok(compilers)
}

#[derive(Deserialize)]
struct CompileCommand {
    directory: String,
    file: String,
    arguments: Option<Vec<String>>,
    command: Option<String>,
}

fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir if normalized.file_name().is_some() => {
                normalized.pop();
            }
            _ => normalized.push(part.as_os_str()),
        }
    }
    normalized
}

fn c_compilers(project: &Project, path: &Path, cancel: &CancelToken) -> io::Result<Vec<Compiler>> {
    let mut compilers = Vec::new();
    for dir in ancestors(project, path) {
        for name in ["compile_commands.json", "build/compile_commands.json"] {
            let database = project.join(dir, name);
            if project.kind(&database)?.is_none() {
                continue;
            }
            let commands: Vec<CompileCommand> =
                serde_json::from_str(&project.read_text(&database)?)
                    .map_err(|err| failure(format!("{}: {err}", database.display())))?;
            for entry in commands {
                let cwd = if Path::new(&entry.directory).is_absolute() {
                    PathBuf::from(entry.directory)
                } else {
                    project.join(database.parent().unwrap(), &entry.directory)
                };
                let file = if Path::new(&entry.file).is_absolute() {
                    PathBuf::from(&entry.file)
                } else {
                    project.join(&cwd, &entry.file)
                };
                if normalize(&file) != normalize(path) {
                    continue;
                }
                let words = entry.arguments.unwrap_or_else(|| {
                    run::shell::split(entry.command.as_deref().unwrap_or_default())
                });
                if let Some((program, args)) = words.split_first() {
                    let name = program
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(program)
                        .to_ascii_lowercase();
                    if ["cl", "cl.exe", "clang-cl", "clang-cl.exe"].contains(&name.as_str()) {
                        continue;
                    }
                    let Ok(args) = assembly_args(args) else {
                        continue;
                    };
                    compilers.push(Compiler {
                        label: if compilers.is_empty() {
                            "Project compile command".into()
                        } else {
                            format!("Project compile command {}", compilers.len() + 1)
                        },
                        program: program.clone(),
                        cwd: cwd.clone(),
                        args,
                        cargo_output: None,
                        rust: false,
                    });
                }
            }
        }
        if !compilers.is_empty() {
            break;
        }
    }
    let cpp = Language::from_path(path) == Some(Language::Cpp);
    for program in if cpp {
        ["clang++", "g++"]
    } else {
        ["clang", "gcc"]
    } {
        let compiler = Compiler {
            label: program.into(),
            program: program.into(),
            cwd: project.root_for(path).path.clone(),
            args: vec![
                "-x".into(),
                if cpp { "c++" } else { "c" }.into(),
                path.to_string_lossy().into_owned(),
            ],
            cargo_output: None,
            rust: false,
        };
        if project
            .run_program_cancellable(
                &compiler.cwd,
                program,
                &["--version".into()],
                &[],
                None,
                Duration::from_secs(10),
                Some(cancel),
            )
            .is_ok_and(|out| out.code == Some(0))
        {
            compilers.push(compiler);
        }
    }
    if compilers.is_empty() {
        return Err(failure(
            "No C/C++ compiler found. Install Clang or GCC on the project's host, or provide compile_commands.json in the project or build directory.",
        ));
    }
    Ok(compilers)
}

/// Preserve include paths, defines, standards and target options, while
/// removing build outputs, dependency generation and incompatible emit modes.
fn assembly_args(args: &[String]) -> io::Result<Vec<String>> {
    let mut result = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg.starts_with('@') {
            return Err(failure(
                "Assembly inspection does not support response files in compile_commands.json. Select Clang/GCC and supply the required flags instead.",
            ));
        }
        if ["-o", "-MF", "-MT", "-MQ", "-MJ", "--serialize-diagnostics"].contains(&arg.as_str()) {
            args.next();
            continue;
        }
        if [
            "-c",
            "-S",
            "-E",
            "-M",
            "-MM",
            "-MD",
            "-MMD",
            "-MP",
            "-MG",
            "-emit-llvm",
            "-fsyntax-only",
            "-save-temps",
        ]
        .contains(&arg.as_str())
            || [
                "-MF",
                "-MT",
                "-MQ",
                "-MJ",
                "-flto",
                "-save-temps=",
                "--output=",
            ]
            .iter()
            .any(|prefix| arg.starts_with(prefix))
            || (arg.starts_with("-o") && !arg.starts_with("-objc"))
        {
            continue;
        }
        result.push(arg.clone());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(name: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("nc-assembly-{name}-{}-{nonce}", std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }
        fn project(&self) -> Project {
            Project::local(self.0.clone())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn removes_build_outputs_and_dependency_flags_but_preserves_compile_context() {
        let args = run::shell::split(
            "-I 'include dir' -DVALUE=3 -std=c++20 -c 'a file.cpp' -o a.o -MMD -MF deps.d -MT target -MQ quoted -MJ fragment.json -flto=thin -save-temps=obj -emit-llvm",
        );
        assert_eq!(
            assembly_args(&args).unwrap(),
            ["-I", "include dir", "-DVALUE=3", "-std=c++20", "a file.cpp"]
        );
        assert!(assembly_args(&["@compiler.rsp".into()]).is_err());
    }

    #[test]
    fn compiles_c_and_cpp_and_reports_compiler_errors() {
        for (name, source) in [
            ("square.c", "int square(int x) { return x * x; }"),
            (
                "square.cpp",
                "template<class T> T twice(T x) { return x+x; }\nint square(int x) { return twice(x)*x; }",
            ),
        ] {
            let fixture = Fixture::new(name);
            let path = fixture.write(name, source);
            let project = fixture.project();
            let cancel = CancelToken::new();
            let compilers = match discover(&project, &path, &cancel) {
                Ok(compilers) => compilers,
                Err(err) if err.to_string().starts_with("No C/C++ compiler found") => {
                    eprintln!("Skipping {name}: {err}");
                    continue;
                }
                Err(err) => panic!("{err}"),
            };
            for compiler in compilers {
                let result = compile(&project, &compiler, 2, "", &cancel).unwrap();
                assert!(result.assembly.contains("square"), "{}", result.assembly);
                assert!(result.command.contains("-O2"));
                fixture.write(name, "this is not valid C or C++ !!!");
                let error = compile(&project, &compiler, 0, "", &cancel)
                    .err()
                    .unwrap()
                    .to_string();
                assert!(error.contains("error"), "{error}");
                fixture.write(name, source);
            }
        }
    }

    #[test]
    fn compilation_database_keeps_includes_defines_and_existing_object_files() {
        let fixture = Fixture::new("compile commands with spaces");
        let path = fixture.write(
            "source/square.c",
            "#include \"value.h\"\nint square(int x) { return x * VALUE * SCALE; }",
        );
        fixture.write("include dir/value.h", "#define VALUE 3\n");
        let object = fixture.write("build/square.o", "existing object");
        let database = serde_json::json!([{
            "directory": fixture.0.join("build"), "file": "../source/square.c",
            "arguments": ["gcc", "-I../include dir", "-DSCALE=2", "-c", "../source/square.c", "-o", "square.o", "-MD", "-MF", "square.d"]
        }]);
        fixture.write("build/compile_commands.json", &database.to_string());
        let project = fixture.project();
        let cancel = CancelToken::new();
        if project
            .run_program(
                "gcc",
                &["--version".into()],
                &[],
                None,
                Duration::from_secs(10),
            )
            .is_err()
        {
            eprintln!("Skipping compilation database execution: GCC is unavailable");
            return;
        }
        let compilers = discover(&project, &path, &cancel).unwrap();
        assert_eq!(compilers[0].label, "Project compile command");
        assert!(
            compile(&project, &compilers[0], 2, "", &cancel)
                .unwrap()
                .assembly
                .contains("square")
        );
        assert_eq!(std::fs::read_to_string(object).unwrap(), "existing object");
        assert!(!fixture.0.join("build/square.d").exists());
    }

    #[test]
    fn compiles_standalone_rust_as_a_library() {
        let fixture = Fixture::new("standalone");
        let path = fixture.write(
            "square.rs",
            "#[unsafe(no_mangle)] pub extern \"C\" fn square(x: i32) -> i32 { x*x }",
        );
        let project = fixture.project();
        let cancel = CancelToken::new();
        let compilers = discover(&project, &path, &cancel).unwrap();
        assert!(compilers[0].cargo_output.is_none());
        assert!(
            compile(&project, &compilers[0], 2, "", &cancel)
                .unwrap()
                .assembly
                .contains("square")
        );
    }

    #[test]
    fn cargo_compiles_the_containing_target_with_dependencies_and_refreshes_assembly() {
        let fixture = Fixture::new("cargo");
        fixture.write(
            "Cargo.toml",
            "[workspace]\nmembers=[\"app\",\"dep\"]\nresolver=\"3\"\n",
        );
        fixture.write(
            "dep/Cargo.toml",
            "[package]\nname=\"assembly_dep\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
        );
        fixture.write("dep/src/lib.rs", "pub fn value(x: i32) -> i32 { x*2 }\n");
        fixture.write("app/Cargo.toml", "[package]\nname=\"assembly_app\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\nassembly_dep={path=\"../dep\"}\n");
        fixture.write("app/src/lib.rs", "mod inner; pub use inner::square;\n");
        fixture.write(
            "app/src/main.rs",
            "fn main() { println!(\"{}\", assembly_app::square(2)); }\n",
        );
        let path = fixture.write("app/src/inner.rs", "#[unsafe(no_mangle)] pub extern \"C\" fn square(x: i32) -> i32 { assembly_dep::value(x) * x }\n");
        let project = fixture.project();
        let cancel = CancelToken::new();
        let compilers = discover(&project, &path, &cancel).unwrap();
        assert_eq!(compilers[0].label, "assembly_app · lib");
        let first = compile(&project, &compilers[0], 2, "", &cancel).unwrap();
        assert!(first.assembly.contains("square"));
        fixture.write("app/src/inner.rs", "#[unsafe(no_mangle)] pub extern \"C\" fn twice(x: i32) -> i32 { assembly_dep::value(x) }\npub fn square(x: i32) -> i32 { twice(x)*x }\n");
        let second = compile(&project, &compilers[0], 2, "", &cancel).unwrap();
        assert!(second.assembly.contains("twice"));
        assert_ne!(first.assembly, second.assembly);
        assert_eq!(
            std::fs::read_dir(compilers[0].cargo_output.as_ref().unwrap())
                .unwrap()
                .count(),
            0,
            "temporary assembly files are removed"
        );
        let targets = discover(&project, &fixture.0.join("app/src/main.rs"), &cancel).unwrap();
        assert_eq!(targets[0].label, "assembly_app · bin assembly_app");
        assert!(
            compile(&project, &targets[0], 0, "", &cancel)
                .unwrap()
                .assembly
                .contains("main")
        );
    }
}
