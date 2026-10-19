// No console window for the editor; command-line output still works (see
// `startup::attach_console`).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app_state;
mod branches_popup;
mod call_hierarchy;
mod change_tree;
mod command_palette;
mod commands;
mod commit_panel;
mod compare_view;
mod connecting;
mod debugger;
mod diff_view;
mod editor;
mod file_finder;
mod file_icons;
mod file_ops;
mod file_tree;
mod file_types;
mod git_dialogs;
mod git_log;
mod headless;
mod instance;
mod launch;
mod layout;
mod location_picker;
mod lsp_store;
mod menus;
mod outline_panel;
mod notebook;
mod note_store;
mod notes_panel;
mod ports;
mod problems;
mod pinpoint;
mod pinpoint_text;
mod project;
mod project_picker;
mod python_panel;
mod recent;
mod revision_picker;
mod run_config;
mod search_panel;
mod selectable;
mod settings;
mod shelf_view;
mod shell;
mod startup;
mod stash_dialogs;
mod symbol_picker;
mod terminal_panel;
mod test_explorer;
mod theme;
mod theme_picker;
mod workflows;
mod workspace;
mod workspace_edit;
mod workspace_file;
mod worktree_picker;

use std::ffi::OsString;

use anyhow::Context as _;
use gpui::{App, Application};

use crate::project::Location;

fn main() -> anyhow::Result<()> {
    startup::init();
    instance::init();
    let result = run();
    if result.is_err() {
        // Started from a shell: show the error there.
        startup::attach_console();
    }
    result
}

/// What the window(s) at startup show.
enum Launch {
    /// The project on the command line, alone (as in VS Code).
    Open(Location),
    /// The last session's windows (none: the welcome screen).
    Restore(app_state::AppState),
}

fn run() -> anyhow::Result<()> {
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // `next_code context …` is `next_code pinpoint context …`.
    if let Some(skip) = args.first().and_then(|arg| match arg.to_str() {
        Some("pinpoint") => Some(1),
        Some("context") => Some(0),
        _ => None,
    }) {
        // A one-shot client of the running editor: no GUI here.
        startup::attach_console();
        let args: Vec<String> = args[skip..]
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        std::process::exit(pinpoint::server::run_cli(&args));
    }
    if args.first().is_some_and(|arg| arg == "notes") {
        // Edits the project's notes file directly: no GUI, no running editor.
        startup::attach_console();
        let args: Vec<String> = args[1..]
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        std::process::exit(notes::cli::run(&args, "next_code"));
    }
    if args.first().is_some_and(|arg| arg == "--automation") {
        startup::attach_console();
        let status = std::process::Command::new("python3")
            .arg("-c")
            .arg(include_str!("../../../scripts/automation.py"))
            .arg("--binary")
            .arg(std::env::current_exe()?)
            .args(&args[1..])
            .status()
            .context("starting UI automation (requires Linux and Python 3)")?;
        anyhow::ensure!(status.success(), "UI automation exited with {status}");
        return Ok(());
    }
    let headless = args.first().is_some_and(|arg| arg == "--headless");
    if headless {
        args.remove(0);
    }
    // A separate editor process: neither joins nor becomes the shared one.
    let standalone = args.iter().position(|arg| arg == "--new-instance");
    if let Some(index) = standalone {
        args.remove(index);
        instance::set_standalone();
    }
    if headless {
        startup::attach_console();
        let location = launch::resolve(&args, None)?;
        headless::run(match location {
            Some(location) => location,
            None => launch::checked(launch::current_dir(), None)?,
        });
        return Ok(());
    }
    // The editor already running takes the launch (and `--quit`).
    if standalone.is_none()
        && let Some(code) = instance::forward(&args)
    {
        std::process::exit(code);
    }
    if args.first().is_some_and(|arg| arg == "--quit") {
        return Ok(());
    }
    let location = launch::resolve(&args, None)?;
    let launch = match location {
        Some(location) => Launch::Open(location),
        None => Launch::Restore(app_state::restorable()),
    };

    startup::set_app_id();
    // Connect to WSL or read git status while gpui sets up the platform.
    let preload = match &launch {
        Launch::Open(location) => Some(location),
        Launch::Restore(state) => state.focused_tab(),
    }
    .map(workspace::Preload::start);
    let application = Application::new().with_assets(file_icons::Assets);
    startup::mark("gpui platform");
    application.run(move |cx: &mut App| {
        startup::mark("gpui ready");
        editor::bind_keys(cx);
        selectable::bind_keys(cx);
        shell::bind_keys(cx);
        command_palette::bind_keys(cx);
        workspace::bind_keys(cx);
        search_panel::bind_keys(cx);
        file_finder::bind_keys(cx);
        file_tree::bind_keys(cx);
        location_picker::bind_keys(cx);
        call_hierarchy::bind_keys(cx);
        outline_panel::bind_keys(cx);
        symbol_picker::bind_keys(cx);
        project_picker::bind_keys(cx);
        theme_picker::bind_keys(cx);
        terminal_panel::bind_keys(cx);
        ports::panel::bind_keys(cx);
        git_log::bind_keys(cx);
        workflows::bind_keys(cx);
        // After `editor::bind_keys`: cell editors' keys win over the editor's.
        notebook::view::bind_keys(cx);
        notebook::picker::bind_keys(cx);
        theme::refresh(cx);
        app_state::init(cx);
        pinpoint::init(cx);
        cx.on_window_closed(|cx| {
            // `app_state` decided first (its hook is older) whether the
            // editor stays for the next launch.
            if cx.windows().is_empty() && !instance::stays_resident() {
                cx.quit();
            }
        })
        .detach();
        startup::close_on_termination(cx);
        match launch {
            Launch::Open(location) => shell::open_window(location, preload, cx),
            Launch::Restore(state) => shell::restore(state, preload, cx),
        }
        startup::mark("window opened");
        cx.activate(true);
        // Later launches reach this process from here on.
        instance::serve(cx);
    });
    Ok(())
}
