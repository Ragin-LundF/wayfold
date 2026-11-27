//! U15: the theme toggle flips light/dark and persists `light_theme` in the temp data dir.

use super::harness::{Env, open, settle_until};
use crate::app::actions::ToggleTheme;
use crate::ui::workspace::{Prefs, Workspace};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{Entity, TestAppContext, VisualTestContext};
use wayfold_core::storage;

fn toggle_and_check(ws: &Entity<Workspace>, cx: &mut VisualTestContext, light: bool) {
    cx.dispatch_action(ToggleTheme);
    assert_eq!(ws.read_with(cx, |w, _| w.prefs.light_theme), light);
    assert_eq!(cx.update(|_, cx| cx.theme().is_dark()), !light);
    // Prefs are saved on a pool thread; startup (`main`) reads them back with `storage::load`.
    settle_until(ws, cx, "prefs saved", |_| storage::load::<Prefs>("prefs").light_theme == light);
}

#[gpui_kit::test]
fn u15_theme_toggle_switches_mode_and_persists_prefs(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    let (ws, cx) = open(cx, &env);
    assert_eq!(storage::data_dir(), env.root.join("data"));
    assert!(!ws.read_with(cx, |w, _| w.prefs.light_theme));

    toggle_and_check(&ws, cx, true);
    toggle_and_check(&ws, cx, false);
    toggle_and_check(&ws, cx, true);

    // A new Workspace (as on the next start) restores the saved preference.
    let (reopened, cx) = open(cx, &env);
    assert!(reopened.read_with(cx, |w, _| w.prefs.light_theme));
}
