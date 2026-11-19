use editor::{Editor, test::editor_content_with_blocks};
use fs::FakeFs;
use gpui::TestAppContext;
use project::Project;
use serde_json::json;
use settings::SettingsStore;
use util::path;

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
    json!({ "version": 1, "files": { "a.rs": comments } }).to_string()
}

#[gpui::test]
async fn test_comments_show_under_their_lines_and_follow_the_store(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({
            "a.rs": "one\ntwo\nthree\nfour\n",
            ".tmp": {
                "line-comment.json": store_text(json!([
                    { "line": 1, "end_line": 2, "hash": "", "text": "check\nthis" },
                    { "line": 3, "hash": "", "text": "mine", "author": "agent", "orphaned": true },
                ])),
            },
        }),
    )
    .await;
    let project = Project::test(fs.clone(), [path!("/root").as_ref()], cx).await;
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(path!("/root/a.rs"), cx)
        })
        .await
        .unwrap();
    let (editor, cx) = cx.add_window_view(|window, cx| {
        Editor::for_buffer(buffer, Some(project), window, cx)
    });
    cx.run_until_parked();
    assert_eq!(
        editor_content_with_blocks(&editor, cx),
        "one\ntwo\n§ check this\nthree\n§ Claude: mine (orphaned)\nfour"
    );

    fs.insert_file(
        path!("/root/.tmp/line-comment.json"),
        store_text(json!([{ "line": 4, "hash": "", "text": "last" }])).into_bytes(),
    )
    .await;
    cx.run_until_parked();
    assert_eq!(
        editor_content_with_blocks(&editor, cx),
        "one\ntwo\nthree\nfour\n§ last"
    );
}
