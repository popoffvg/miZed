use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use editor::{
    Editor, MultiBuffer, PathKey,
    actions::{Backspace, Newline, NewlineBelow, SelectAll},
    display_map::{Block, BlockId},
    test::editor_content_with_blocks,
};
use fs::{FakeFs, Fs as _};
use gpui::{
    Entity, Focusable as _, MouseButton, TestAppContext, VisualContext as _, VisualTestContext,
};
use language::Point;
use project::Project;
use serde_json::json;
use settings::SettingsStore;
use util::path;
use workspace::MultiWorkspace;

use crate::{
    AddLineComment, Author, CommentStoreName, EditLineComment, LineComment, ListLineComments,
    SelectLineCommentStore, comment_store::STORE_VERSION,
    comment_store_path::line_comment_store_path,
};

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
    json!({ "version": STORE_VERSION, "files": { FILE: comments } }).to_string()
}

async fn stored_comments(fs: &FakeFs, store_path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs.load(store_path).await.unwrap()).unwrap()
}

/// Opens a file in an editor of a saved workspace on `/root`, and returns the path of its
/// comment store.
pub(super) async fn open_file(
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
        .read_with(&cx, |multi_workspace, _| {
            multi_workspace.workspace().clone()
        })
        .unwrap();
    let workspace_id = workspace.update(&mut cx, |workspace, _| {
        workspace.set_random_database_id();
        workspace.database_id().unwrap()
    });
    let store_path = line_comment_store_path(workspace_id.into(), &CommentStoreName::default());
    if let Some(store) = store {
        fs.create_dir(store_path.parent().unwrap()).await.unwrap();
        fs.insert_file(&store_path, store_text(store).into_bytes())
            .await;
    }
    let buffer = project
        .update(&mut cx, |project, cx| project.open_local_buffer(file, cx))
        .await
        .unwrap();
    let editor =
        cx.new_window_entity(|window, cx| Editor::for_buffer(buffer, Some(project), window, cx));
    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx)
    });
    cx.run_until_parked();
    (fs, editor, store_path, cx)
}

#[gpui::test(iterations = 12)]
async fn test_overlapping_comment_saves_keep_both_comments(cx: &mut TestAppContext) {
    let (fs, editor, store_path, mut cx) = open_file(FILE, None, cx).await;
    let store = editor.read_with(&cx, |editor, _| {
        crate::editor_comment_store(editor).unwrap()
    });
    let (first, second) = store.update(&mut cx, |store, cx| {
        let add = |store: &mut crate::CommentStore, line, cx: &mut gpui::Context<_>| {
            store.add_comment(
                FILE,
                LineComment {
                    line,
                    end_line: None,
                    text: line.to_string(),
                    done: false,
                    orphaned: false,
                    author: Author::Human,
                },
                cx,
            )
        };
        (add(store, 1, cx), add(store, 2, cx))
    });
    first.await.unwrap();
    second.await.unwrap();
    assert_eq!(
        stored_comments(&fs, &store_path).await["files"][FILE]
            .as_array()
            .unwrap()
            .iter()
            .map(|comment| comment["line"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [1, 2]
    );
    cx.run_until_parked();
    assert_eq!(
        store.read_with(&cx, |store, _| store.comments(FILE).len()),
        2
    );
}

#[gpui::test]
async fn test_done_comment_retries_after_save_failure(cx: &mut TestAppContext) {
    let (fs, editor, store_path, mut cx) =
        open_file(FILE, Some(json!([{ "line": 1, "text": "review" }])), cx).await;
    let store = editor.read_with(&cx, |editor, _| {
        crate::editor_comment_store(editor).unwrap()
    });
    let original = fs.load(&store_path).await.unwrap();
    fs.pause_events();
    fs.remove_file(&store_path, Default::default())
        .await
        .unwrap();
    fs.create_dir(&store_path).await.unwrap();
    let save = |cx: &mut VisualTestContext| {
        store.update(cx, |store, cx| store.set_comment_done(FILE, 1, true, cx))
    };
    assert!(save(&mut cx).await.is_err());
    fs.remove_dir(&store_path, Default::default())
        .await
        .unwrap();
    fs.atomic_write(store_path.clone(), original).await.unwrap();
    save(&mut cx).await.unwrap();
    assert_eq!(
        stored_comments(&fs, &store_path).await["files"][FILE][0]["done"],
        true
    );
    fs.unpause_events_and_flush();
}

#[gpui::test]
async fn test_comment_save_preserves_external_changes_before_reload(cx: &mut TestAppContext) {
    let (fs, editor, store_path, mut cx) =
        open_file(FILE, Some(json!([{ "line": 1, "text": "review" }])), cx).await;
    let store = editor.read_with(&cx, |editor, _| {
        crate::editor_comment_store(editor).unwrap()
    });
    fs.pause_events();
    fs.atomic_write(
        store_path.clone(),
        store_text(json!([
            { "line": 1, "text": "review" },
            { "line": 2, "text": "external" },
        ])),
    )
    .await
    .unwrap();
    store
        .update(&mut cx, |store, cx| {
            store.set_comment_done(FILE, 1, true, cx)
        })
        .await
        .unwrap();
    let comments = stored_comments(&fs, &store_path).await;
    assert_eq!(comments["files"][FILE][0]["done"], true);
    assert_eq!(comments["files"][FILE][1]["text"], "external");
    fs.atomic_write(
        store_path.clone(),
        store_text(json!([{ "line": 1, "text": "external edit", "done": true }])),
    )
    .await
    .unwrap();
    let save = store.update(&mut cx, |store, cx| {
        store.set_comment_done(FILE, 1, false, cx)
    });
    assert!(save.await.is_err());
    assert_eq!(
        store.read_with(&cx, |store, _| store.comments(FILE)[0].text.clone()),
        "external edit"
    );
    fs.unpause_events_and_flush();
}

#[gpui::test]
async fn test_comment_spanning_past_excerpt_shows_in_excerpt(cx: &mut TestAppContext) {
    let (_, editor, _, mut cx) = open_file(
        FILE,
        Some(json!([{ "line": 1, "end_line": 4, "text": "review" }])),
        cx,
    )
    .await;
    let workspace = editor.read_with(&cx, |editor, _| editor.workspace().unwrap());
    let project = editor.read_with(&cx, |editor, _| editor.project().unwrap().clone());
    let buffer = project
        .update(&mut cx, |project, cx| project.open_local_buffer(FILE, cx))
        .await
        .unwrap();
    let next_buffer = project
        .update(&mut cx, |project, cx| {
            project.open_local_buffer(LOOSE_FILE, cx)
        })
        .await
        .unwrap();
    let multibuffer = cx.new_window_entity(|_, cx| {
        let mut multibuffer = MultiBuffer::new(project.read(cx).capability());
        multibuffer.set_excerpts_for_path(
            PathKey::sorted(0),
            buffer,
            [Point::new(1, 0)..Point::new(2, 5)],
            0,
            cx,
        );
        multibuffer.set_excerpts_for_path(
            PathKey::sorted(1),
            next_buffer,
            [Point::new(0, 0)..Point::new(0, 3)],
            0,
            cx,
        );
        multibuffer
    });
    let excerpt = cx.new_window_entity(|window, cx| {
        Editor::for_multibuffer(multibuffer, Some(project), window, cx)
    });
    workspace.update_in(&mut cx, |workspace, window, cx| {
        workspace.add_item_to_active_pane(Box::new(excerpt.clone()), None, true, window, cx);
    });
    cx.run_until_parked();
    excerpt.read_with(&cx, |editor, cx| {
        assert_eq!(
            editor
                .highlighted_rows::<crate::LineCommentTargetHighlights>(cx)
                .count(),
            1
        );
    });
    let content = editor_content_with_blocks(&excerpt, &mut cx);
    assert!(content.contains("§ review"), "{content}");
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
        "one\ntwo\n§ check\n§ this\nthree\n§ Claude: mine (orphaned)\nfour"
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
async fn test_inline_comment_wraps_at_editor_width(cx: &mut TestAppContext) {
    let text = "This comment needs room to wrap. ".repeat(20);
    let (_, editor, _, mut cx) =
        open_file(FILE, Some(json!([{ "line": 1, "text": text }])), cx).await;
    editor.update(&mut cx, |editor, cx| {
        let block_id = editor.addon::<crate::LineCommentAddon>().unwrap().block_ids[0];
        let Some(Block::Custom(block)) = editor
            .display_snapshot(cx)
            .block_for_id(BlockId::Custom(block_id))
        else {
            panic!("The line comment block must exist");
        };
        assert!(block.height.unwrap() > 1);
    });
}

#[gpui::test]
async fn test_comment_icon_toggles_and_its_menu_edits(cx: &mut TestAppContext) {
    let (fs, editor, store_path, mut cx) =
        open_file(FILE, Some(json!([{ "line": 3, "text": "rename" }])), cx).await;
    assert!(editor_content_with_blocks(&editor, &mut cx).contains("§ rename"));

    let icon = |cx: &mut VisualTestContext| {
        cx.debug_bounds("toggle-line-comment")
            .expect("The comment icon must remain visible")
            .center()
    };
    let position = icon(&mut cx);
    cx.simulate_click(position, Default::default());
    cx.run_until_parked();
    assert_eq!(
        stored_comments(&fs, &store_path).await["files"][FILE][0]["done"],
        true
    );
    assert!(editor_content_with_blocks(&editor, &mut cx).contains("§ rename"));

    let position = icon(&mut cx);
    cx.simulate_click(position, Default::default());
    cx.run_until_parked();
    assert!(
        stored_comments(&fs, &store_path).await["files"][FILE][0]
            .get("done")
            .is_none()
    );
    assert!(editor_content_with_blocks(&editor, &mut cx).contains("§ rename"));

    let position = icon(&mut cx);
    cx.simulate_mouse_move(position, None, Default::default());
    cx.simulate_mouse_down(position, MouseButton::Right, Default::default());
    cx.simulate_mouse_up(position, MouseButton::Right, Default::default());
    cx.run_until_parked();
    let edit = cx
        .debug_bounds("MENU_ITEM-Edit Line Comment")
        .expect("The comment menu must contain an edit action");
    cx.simulate_click(edit.center(), Default::default());
    cx.run_until_parked();
    cx.dispatch_action(SelectAll);
    cx.simulate_input("renamed");
    cx.dispatch_action(NewlineBelow);
    cx.run_until_parked();
    assert_eq!(
        stored_comments(&fs, &store_path).await["files"][FILE][0]["text"],
        "renamed"
    );
}

#[gpui::test]
async fn test_list_line_comments_searches_and_opens_the_matching_line(cx: &mut TestAppContext) {
    let (_, editor, _, mut cx) = open_file(
        FILE,
        Some(json!([
            { "line": 1, "text": "check imports" },
            { "line": 3, "text": "rename variable" },
        ])),
        cx,
    )
    .await;
    editor.update_in(&mut cx, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.dispatch_action(ListLineComments);
    cx.run_until_parked();
    cx.simulate_input("rename");
    cx.run_until_parked();
    cx.update(|window, cx| {
        let confirm = cx.build_action("menu::Confirm", None).unwrap();
        window.dispatch_action(confirm, cx);
    });
    cx.run_until_parked();
    editor.update(&mut cx, |editor, cx| {
        let selection = editor
            .selections
            .newest_adjusted(&editor.display_snapshot(cx));
        assert_eq!(selection.start, Point::new(2, 0));
        assert_eq!(selection.end, selection.start);
    });
}

#[gpui::test]
async fn test_prompt_writes_changes_and_deletes_a_comment(cx: &mut TestAppContext) {
    let (fs, editor, store_path, mut cx) = open_file(FILE, None, cx).await;
    let submit_key = if cfg!(target_os = "macos") {
        "cmd-enter"
    } else {
        "ctrl-enter"
    };
    cx.update(|_, cx| {
        cx.bind_keys([
            gpui::KeyBinding::new("enter", Newline, Some("MultilineTextPrompt > Editor")),
            gpui::KeyBinding::new(
                submit_key,
                NewlineBelow,
                Some("MultilineTextPrompt > Editor"),
            ),
        ]);
    });
    let select_lines = |range, cx: &mut VisualTestContext| {
        editor.update_in(cx, |editor, window, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_ranges([range])
            });
            window.focus(&editor.focus_handle(cx), cx);
        });
    };
    let submit = |text: &str, cx: &mut VisualTestContext| {
        cx.run_until_parked();
        cx.dispatch_action(SelectAll);
        cx.dispatch_action(Backspace);
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                cx.simulate_keystrokes("enter");
            }
            cx.simulate_input(line);
        }
        cx.simulate_keystrokes(submit_key);
        cx.run_until_parked();
    };

    select_lines(Point::new(1, 0)..Point::new(3, 0), &mut cx);
    cx.dispatch_action(AddLineComment);
    submit("rename\nwith reason", &mut cx);
    assert_eq!(
        stored_comments(&fs, &store_path).await,
        json!({ "version": STORE_VERSION, "files": { FILE: [
            { "line": 2, "end_line": 3, "text": "rename\nwith reason", "orphaned": false, "author": "human" },
        ] } })
    );
    assert_eq!(
        editor_content_with_blocks(&editor, &mut cx),
        "one\ntwo\nthree\n§ rename\n§ with reason\nfour"
    );
    editor.read_with(&cx, |editor, cx| {
        use editor::ToPoint as _;
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let targets: Vec<_> = editor
            .highlighted_rows::<crate::LineCommentTargetHighlights>(cx)
            .map(|(range, _)| range.start.to_point(&snapshot)..range.end.to_point(&snapshot))
            .collect();
        assert_eq!(targets, [Point::new(1, 0)..Point::new(2, 5)]);
    });

    let store = editor.read_with(&cx, |editor, _| {
        crate::editor_comment_store(editor).unwrap()
    });
    store
        .update(&mut cx, |store, cx| {
            store.set_comment_done(FILE, 2, true, cx)
        })
        .await
        .unwrap();
    assert_eq!(
        stored_comments(&fs, &store_path).await["files"][FILE][0]["done"],
        true
    );

    select_lines(Point::new(2, 0)..Point::new(2, 0), &mut cx);
    cx.dispatch_action(EditLineComment);
    submit("keep", &mut cx);
    assert_eq!(
        stored_comments(&fs, &store_path).await["files"][FILE][0],
        json!({ "line": 2, "end_line": 3, "text": "keep", "done": true, "orphaned": false, "author": "human" }),
    );

    store
        .update(&mut cx, |store, cx| {
            store.set_comment_done(FILE, 2, false, cx)
        })
        .await
        .unwrap();
    assert!(
        stored_comments(&fs, &store_path).await["files"][FILE][0]
            .get("done")
            .is_none()
    );

    cx.dispatch_action(EditLineComment);
    submit("", &mut cx);
    assert_eq!(
        stored_comments(&fs, &store_path).await,
        json!({ "version": STORE_VERSION, "files": {} })
    );
    assert_eq!(
        editor_content_with_blocks(&editor, &mut cx),
        "one\ntwo\nthree\nfour"
    );
    editor.read_with(&cx, |editor, cx| {
        assert_eq!(
            editor
                .highlighted_rows::<crate::LineCommentTargetHighlights>(cx)
                .count(),
            0
        );
    });
}

#[gpui::test]
async fn test_named_comment_store_keeps_separate_comments(cx: &mut TestAppContext) {
    let (fs, editor, default_path, mut cx) = open_file(
        FILE,
        Some(json!([{ "line": 1, "text": "default comment" }])),
        cx,
    )
    .await;
    editor.update_in(&mut cx, |editor, window, cx| {
        window.focus(&editor.focus_handle(cx), cx);
    });
    let select_store = |name: &str, cx: &mut VisualTestContext| {
        cx.dispatch_action(SelectLineCommentStore);
        cx.run_until_parked();
        cx.dispatch_action(SelectAll);
        cx.simulate_input(name);
        cx.dispatch_action(Newline);
        cx.run_until_parked();
    };

    select_store("review", &mut cx);
    assert!(!editor_content_with_blocks(&editor, &mut cx).contains("default comment"));
    cx.dispatch_action(AddLineComment);
    cx.run_until_parked();
    cx.simulate_input("review comment");
    cx.dispatch_action(NewlineBelow);
    cx.run_until_parked();
    assert!(editor_content_with_blocks(&editor, &mut cx).contains("review comment"));

    let named_path = default_path.with_extension("").join("store-review.json");
    assert_eq!(
        stored_comments(&fs, &named_path).await["files"][FILE][0]["text"],
        "review comment"
    );
    select_store("default", &mut cx);
    let content = editor_content_with_blocks(&editor, &mut cx);
    assert!(content.contains("default comment"));
    assert!(!content.contains("review comment"));

    select_store("review", &mut cx);
    assert!(editor_content_with_blocks(&editor, &mut cx).contains("review comment"));
}

#[gpui::test]
async fn test_file_outside_the_workspace_folders_uses_the_workspace_store(cx: &mut TestAppContext) {
    let (fs, editor, store_path, mut cx) = open_file(LOOSE_FILE, None, cx).await;
    editor.update_in(&mut cx, |editor, window, cx| {
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([Point::new(0, 0)..Point::new(0, 0)])
        });
        window.focus(&editor.focus_handle(cx), cx);
    });
    cx.dispatch_action(AddLineComment);
    cx.run_until_parked();
    cx.simulate_input("tighten");
    cx.dispatch_action(NewlineBelow);
    cx.run_until_parked();
    assert_eq!(
        stored_comments(&fs, &store_path).await,
        json!({ "version": STORE_VERSION, "files": { LOOSE_FILE: [
            { "line": 1, "text": "tighten", "orphaned": false, "author": "human" },
        ] } })
    );
    assert_eq!(
        editor_content_with_blocks(&editor, &mut cx),
        "one\n§ tighten\ntwo\nthree\nfour"
    );
}
