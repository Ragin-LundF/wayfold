//! U06: localized Finder tags (D-3). A synthetic Finder prefs file in the temp HOME names the
//! standard tags in German, so the real user prefs are never read.

use super::harness::{Env, open};
use crate::ui::chrome::tag_aria_label;
use crate::ui::fileops::{all_have_standard_tag, set_standard_tag};
use crate::ui::search::tag_matches;
use crate::ui::workspace::Workspace;
use gpui_kit::{Entity, Hsla, TestAppContext, VisualTestContext, rgb};
use wayfold_core::fs::tags::{self, Tag};

const GERMAN_FINDER_PREFS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>FavoriteTagNames</key><array>
<string></string><string>Rot</string><string>Orange</string><string>Gelb</string><string>Grün</string>
<string>Blau</string><string>Lila</string><string>Grau</string>
</array></dict></plist>"#;

fn german_env(files: &[&str]) -> Env {
    let env = Env::new(files);
    let prefs = env.root.join("home/Library/Preferences");
    std::fs::create_dir_all(&prefs).unwrap();
    std::fs::write(prefs.join("com.apple.finder.plist"), GERMAN_FINDER_PREFS).unwrap();
    env
}

fn tag(name: &str, color: u8) -> Tag {
    Tag { name: name.into(), color }
}

fn row_tags(ws: &Entity<Workspace>, cx: &mut VisualTestContext, name: &str) -> Vec<Tag> {
    ws.read_with(cx, |w, _| w.rows().iter().find(|e| e.name == name).map(|e| e.tags.clone())).unwrap_or_else(|| panic!("no row {name}"))
}

/// Finder's standard tag for an English key, as the workspace read it at startup.
fn sidebar_tag(ws: &Entity<Workspace>, cx: &mut VisualTestContext, english: &str) -> Tag {
    ws.read_with(cx, |w, _| w.standard_tag(english).cloned()).unwrap_or_else(|| panic!("no sidebar tag {english}"))
}

/// What the sidebar, the tag-filter breadcrumb and the search title show for a filter key.
fn shown_label(ws: &Entity<Workspace>, cx: &mut VisualTestContext, key: &str) -> String {
    ws.read_with(cx, |w, _| w.tag_label(key))
}

#[gpui_kit::test]
fn u06_localized_blue_tag_shows_blue_dot_and_matches_sidebar_blue_only(cx: &mut TestAppContext) {
    let env = german_env(&["blau.txt", "rot.txt", "work.txt"]);
    let work = env.work();
    tags::write(&work.join("blau.txt"), &[tag("Blau", 4)]).unwrap();
    tags::write(&work.join("rot.txt"), &[tag("Rot", 6)]).unwrap();
    tags::write(&work.join("work.txt"), &[tag("Work", 4)]).unwrap();
    let (ws, cx) = open(cx, &env);

    let blau = row_tags(&ws, cx, "blau.txt");
    assert_eq!(blau, [tag("Blau", 4)]);
    let blue_dot: Hsla = rgb(0x68a5ef).into();
    assert_eq!(crate::ui::theme::tag_color(blau[0].color), Some(blue_dot));

    let label = shown_label(&ws, cx, "Blue");
    assert_eq!(label, "Blau", "sidebar shows Finder's localized name");
    assert_eq!(tag_aria_label(&label), "Filter by tag Blau");

    // The tag search's scan fallback rule.
    let blue = sidebar_tag(&ws, cx, "Blue");
    let matches_blue = |file: &str, cx: &mut VisualTestContext| row_tags(&ws, cx, file).iter().any(|t| tag_matches(Some(&blue), "Blue", t));
    assert!(matches_blue("blau.txt", cx));
    assert!(!matches_blue("rot.txt", cx), "red tag is not blue");
    assert!(!matches_blue("work.txt", cx), "custom tag with the blue color is not the Blue tag");
    assert!(tag_matches(None, "Work", &tag("Work", 4)), "a custom key matches by name");
    assert!(!tag_matches(None, "Work", &tag("Blau", 4)), "a custom key ignores the color");

    // The filter key stays English, whatever the label says.
    ws.update(cx, |w, cx| w.toggle_tag_filter("Blue", cx));
    assert_eq!(ws.read_with(cx, |w, _| w.tag_filter.clone()).as_deref(), Some("Blue"));
    assert_eq!(ws.read_with(cx, |w, _| w.search.title.clone()), "Tag: Blau", "search title is localized");
}

#[gpui_kit::test]
fn u06_tags_dialog_chip_adds_and_removes_localized_tag_and_keeps_custom(cx: &mut TestAppContext) {
    let env = german_env(&["a.txt"]);
    let file = env.work().join("a.txt");
    tags::write(&file, &[tag("Work", 4)]).unwrap();
    let (ws, cx) = open(cx, &env);
    let blue = sidebar_tag(&ws, cx, "Blue");

    let other = env.work().join("b.txt");
    std::fs::write(&other, b"").unwrap();
    let chip_selected = |paths: &[&std::path::Path]| {
        let entries: Vec<_> = paths.iter().map(|p| wayfold_core::fs::stat_entry(p)).collect();
        all_have_standard_tag(&entries, &blue, "Blue")
    };
    assert!(!chip_selected(&[&file]), "custom Work does not select the Blue chip");

    set_standard_tag(&file, &blue, "Blue", true).unwrap();
    assert_eq!(tags::read(&file), [tag("Work", 4), tag("Blau", 4)], "custom Work does not count as Blue");
    set_standard_tag(&file, &blue, "Blue", true).unwrap();
    assert_eq!(tags::read(&file).len(), 2, "adding twice keeps one Blau");
    assert!(chip_selected(&[&file]));
    assert!(!chip_selected(&[&file, &other]), "selected only when every item has the tag");
    tags::write(&other, &[tag("Blue", 4)]).unwrap();
    assert!(chip_selected(&[&file, &other]), "the English name counts as the Blue tag");

    set_standard_tag(&file, &blue, "Blue", false).unwrap();
    assert_eq!(tags::read(&file), [tag("Work", 4)]);
}

#[gpui_kit::test]
fn u06_without_finder_prefs_tags_fall_back_to_english(cx: &mut TestAppContext) {
    let env = Env::new(&[]);
    let (ws, cx) = open(cx, &env);
    assert_eq!(sidebar_tag(&ws, cx, "Blue"), tag("Blue", 4));
    assert_eq!(shown_label(&ws, cx, "Blue"), "Blue");
}
