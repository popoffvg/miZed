use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use editor::{
    Editor,
    actions::{Backspace, Newline, SelectAll},
    test::editor_content_with_blocks,
};
use fs::{FakeFs, Fs as _};
use gpui::{Entity, Focusable as _, TestAppContext, VisualContext as _, VisualTestContext};
use language::Point;
use project::Project;
use serde_json::json;
use settings::SettingsStore;
use util::path;
use workspace::MultiWorkspace;

use crate::EditLineComment;

const FILE: &str = path!("/root/a.rs");
const LOOSE_FILE: &str = path!("/skills/notes/SKILL.md");

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        crate::init(cx);
    });
}

fn store_text(comments: serde_json::Value) -> String {
    json!({ "version": 1, "files": { FILE: comments } }).to_string()
}

async fn stored_comments(fs: &FakeFs, store_path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs.load(store_path).await.unwrap()).unwrap()
}

/// Opens a file in an editor of a saved workspace on `/root`, and returns the path of its
/// comment store.
async fn open_file(
    file: &str,
    store: Option<serde_json::Value>,
    cx: &mut TestAppContext,
) -> (Arc<FakeFs>, Entity<Editor>, PathBuf, VisualTestContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), json!({ "a.rs": "one\ntwo\nthree\nfour\n" }))
        .await;
    fs.insert_tree(
        path!("/skills/notes"),
        json!({ "SKILL.md": "one\ntwo\nthree\nfour\n" }),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    let workspace = window
        .read_with(&cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let workspace_id = workspace.update(&mut cx, |workspace, _| {
        workspace.set_random_database_id();
        workspace.database_id().unwrap()
    });
    let store_path = paths::data_dir()
        .join("line_comments")
        .join(format!("{}.json", i64::from(workspace_id)));
    if let Some(store) = store {
        fs.create_dir(store_path.parent().unwrap()).await.unwrap();
        fs.insert_file(&store_path, store_text(store).into_bytes())
            .await;
    }
    let buffer = project
        .update(&mut cx, |project, cx| project.open_local_buffer(file, cx))
        .await
        .unwrap();
    let editor = cx.new_window_entity(|window, cx| {
        Editor::for_buffer(buffer, Some(project), window, cx)
    });
    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx)
    });
    cx.run_until_parked();
    (fs, editor, store_path, cx)
}

#[gpui::test]
async fn test_comments_show_under_their_lines_and_follow_the_store(cx: &mut TestAppContext) {
    let (fs, editor, store_path, mut cx) = open_file(
        FILE,
        Some(json!([
            { "line": 1, "end_line": 2, "text": "check\nthis" },
            { "line": 3, "text": "mine", "author": "agent", "orphaned": true },
        ])),
        cx,
    )
    .await;
    assert_eq!(
        editor_content_with_blocks(&editor, &mut cx),
        "one\ntwo\n§ check this\nthree\n§ Claude: mine (orphaned)\nfour"
    );

    fs.insert_file(
        &store_path,
        store_text(json!([{ "line": 4, "text": "last" }])).into_bytes(),
    )
    .await;
    cx.run_until_parked();
    assert_eq!(
        editor_content_with_blocks(&editor, &mut cx),
        "one\ntwo\nthree\nfour\n§ last"
    );
}

#[gpui::test]
async fn test_prompt_writes_changes_and_deletes_a_comment(cx: &mut TestAppContext) {
    let (fs, editor, store_path, mut cx) = open_file(FILE, None, cx).await;
    let comment = |editor: &Entity<Editor>, text: &str, cx: &mut VisualTestContext| {
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([Point::new(1, 0)..Point::new(3, 0)])
            });
            window.focus(&editor.focus_handle(cx), cx);
        });
        cx.dispatch_action(EditLineComment);
        cx.run_until_parked();
        cx.dispatch_action(SelectAll);
        cx.dispatch_action(Backspace);
        cx.simulate_input(text);
        cx.dispatch_action(Newline);
        cx.run_until_parked();
    };

    comment(&editor, "rename", &mut cx);
    assert_eq!(
        stored_comments(&fs, &store_path).await,
        json!({ "version": 1, "files": { FILE: [
            { "line": 2, "end_line": 3, "text": "rename", "orphaned": false, "author": "human" },
        ] } })
    );
    assert_eq!(
        editor_content_with_blocks(&editor, &mut cx),
        "one\ntwo\nthree\n§ rename\nfour"
    );

    comment(&editor, "", &mut cx);
    assert_eq!(
        stored_comments(&fs, &store_path).await,
        json!({ "version": 1, "files": {} })
    );
    assert_eq!(
        editor_content_with_blocks(&editor, &mut cx),
        "one\ntwo\nthree\nfour"
    );
}

#[gpui::test]
async fn test_file_outside_the_workspace_folders_uses_the_workspace_store(
    cx: &mut TestAppContext,
) {
    let (fs, editor, store_path, mut cx) = open_file(LOOSE_FILE, None, cx).await;
    editor.update_in(&mut cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([Point::new(0, 0)..Point::new(0, 0)])
        });
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.dispatch_action(EditLineComment);
    cx.run_until_parked();
    cx.simulate_input("tighten");
    cx.dispatch_action(Newline);
    cx.run_until_parked();
    assert_eq!(
        stored_comments(&fs, &store_path).await,
        json!({ "version": 1, "files": { LOOSE_FILE: [
            { "line": 1, "text": "tighten", "orphaned": false, "author": "human" },
        ] } })
    );
    assert_eq!(
        editor_content_with_blocks(&editor, &mut cx),
        "one\n§ tighten\ntwo\nthree\nfour"
    );
}
