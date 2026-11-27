//! T-229: Cmd+C then Cmd+V in the same folder copies exactly the selected item. Cmd+C writes the
//! pasteboard; with WAYFOLD_DATA_DIR set the core uses a private test pasteboard, never the user's.

use super::harness::{Env, keys, names, open, settle_until};
use gpui_kit::TestAppContext;

#[gpui_kit::test]
fn t229_copy_paste_in_same_folder_duplicates_the_selected_file(cx: &mut TestAppContext) {
    let env = Env::new(&["a.txt", "b.txt", "c.txt"]);
    let (ws, cx) = open(cx, &env);

    keys(&ws, cx, "down down cmd-c cmd-v");
    settle_until(&ws, cx, "b copy.txt listed", |cx| names(&ws, cx).contains(&"b copy.txt".to_string()));
    assert_eq!(names(&ws, cx), ["a.txt", "b copy.txt", "b.txt", "c.txt"]);

    keys(&ws, cx, "end cmd-c cmd-v");
    settle_until(&ws, cx, "c copy.txt listed", |cx| names(&ws, cx).contains(&"c copy.txt".to_string()));
    assert_eq!(names(&ws, cx), ["a.txt", "b copy.txt", "b.txt", "c copy.txt", "c.txt"]);
}
