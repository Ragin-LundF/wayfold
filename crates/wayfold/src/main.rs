//! Wayfold: macOS-first file manager built with GPUI and gpui-component (no WebView).

mod app;
mod terminal_view;
mod ui;
#[cfg(test)]
mod ui_tests;

use gpui_kit::*;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("WAYFOLD_LOG").unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();

    // Crash-safe journal: remove only temp files left by interrupted operations, and only when
    // no other instance runs. The lock must live until the process exits.
    let _journal_lock = wayfold_core::ops::journal::lock_and_recover();

    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(|cx| {
        gpui_kit::init(cx);
        let prefs: ui::workspace::Prefs = wayfold_core::storage::load("prefs");
        ui::theme::apply(!prefs.light_theme, cx);
        app::actions::bind_keys(cx);
        app::actions::set_menus(cx);
        cx.on_action(|_: &app::actions::Quit, cx| cx.quit());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(760.), px(500.))),
            titlebar: Some(TitlebarOptions { title: Some("Wayfold".into()), ..Default::default() }),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| ui::workspace::Workspace::new(window, cx))).expect("open main window");
        cx.activate(true);
    });
}
