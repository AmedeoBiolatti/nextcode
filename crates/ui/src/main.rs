mod editor;
mod file_tree;
mod theme;
mod workspace;

use std::path::PathBuf;

use anyhow::Context as _;
use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};

use crate::workspace::Workspace;

fn main() -> anyhow::Result<()> {
    let root = match std::env::args_os().nth(1) {
        Some(arg) => PathBuf::from(arg),
        None => std::env::current_dir().context("reading current directory")?,
    };
    let root =
        std::path::absolute(&root).with_context(|| format!("resolving {}", root.display()))?;
    anyhow::ensure!(root.is_dir(), "{} is not a directory", root.display());

    Application::new().run(move |cx: &mut App| {
        editor::bind_keys(cx);
        workspace::bind_keys(cx);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let title = format!("{} — next_code", root.display());
        let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(title.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Workspace::new(root, window, cx)),
        )
        .expect("failed to open window");
        cx.activate(true);
    });
    Ok(())
}
