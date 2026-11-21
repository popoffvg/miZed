use std::{cell::Cell, path::Path, rc::Rc};

use context_server::listener::McpServerTool;
use fs::FakeFs;
use gpui::{TestAppContext, VisualTestContext};
use project::Project;
use serde_json::{Value, json};
use settings::SettingsStore;
use workspace::{MultiWorkspace, pane::CloseActiveItem};
use zed_actions::assistant::FocusAgent;

use super::{
    comment_tools::{AddComment, ClearComments, ListComments},
    file_handoff::OpenMultibuffer,
    workspace_switch::{
        ListBranches, ListRepositories, ListWorkspaces, SwitchBranch, SwitchRepository,
        SwitchWorkspace,
    },
};

fn init_test(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let settings = SettingsStore::test(cx);
        cx.set_global(settings);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    });
}

async fn call<T: McpServerTool>(tool: T, input: Value, cx: &mut gpui::AsyncApp) -> Value {
    let input = serde_json::from_value(input).unwrap();
    let response = tool.run(input, cx).await.unwrap();
    serde_json::to_value(response.structured_content).unwrap()
}

#[gpui::test]
async fn test_mcp_clear_comments_respects_file_and_store(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/project",
        json!({ "first.txt": "first", "second.txt": "second" }),
    )
    .await;
    let project = Project::test(fs, [Path::new("/project")], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    window
        .update(cx, |root, _, cx| root.set_random_database_id(cx))
        .unwrap();
    let mut async_cx = cx.to_async();
    for (store, path) in [
        ("review", "/project/first.txt"),
        ("review", "/project/second.txt"),
        ("default", "/project/first.txt"),
    ] {
        call(
            AddComment,
            json!({ "store": store, "path": path, "line": 1, "text": "Review" }),
            &mut async_cx,
        )
        .await;
    }
    assert_eq!(
        call(
            ClearComments,
            json!({ "store": "review", "path": "/project/first.txt" }),
            &mut async_cx
        )
        .await,
        json!({ "cleared": 1 })
    );
    let remaining = call(ListComments, json!({ "store": "review" }), &mut async_cx).await;
    let comments = remaining["comments"].as_array().unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["path"], "/project/second.txt");
    for cleared in [1, 0] {
        assert_eq!(
            call(ClearComments, json!({ "store": "review" }), &mut async_cx).await,
            json!({ "cleared": cleared })
        );
    }
    assert_eq!(
        call(ListComments, json!({ "store": "review" }), &mut async_cx).await,
        json!({ "comments": [] })
    );
    let default_store = call(ListComments, json!({}), &mut async_cx).await;
    assert_eq!(default_store["comments"][0]["path"], "/project/first.txt");
}

#[gpui::test]
async fn test_mcp_focus_chat_after_tab_close(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree("/project", json!({ "file.txt": "first\nsecond\n" }))
        .await;
    let project = Project::test(fs, [Path::new("/project")], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let mut window_cx = VisualTestContext::from_window(window.into(), cx);
    let workspace = window
        .read_with(&window_cx, |root, _| root.workspace().clone())
        .unwrap();
    let focused_chat = Rc::new(Cell::new(false));
    workspace.update(&mut window_cx, |workspace, _| {
        let focused_chat = focused_chat.clone();
        workspace.register_action(move |_, _: &FocusAgent, _, _| focused_chat.set(true));
    });
    let completed = Rc::new(Cell::new(false));
    let input = json!({
        "title": "Review",
        "excerpts": [{ "path": "/project/file.txt", "start_line": 1, "end_line": 1 }],
        "focus_chat_on_close": true,
    });
    let task = cx.spawn({
        let completed = completed.clone();
        async move |mut cx| {
            let result = call(OpenMultibuffer, input, &mut cx).await;
            completed.set(true);
            result
        }
    });
    window_cx.run_until_parked();
    assert!(!completed.get());
    assert!(!focused_chat.get());
    let pane = workspace.read_with(&window_cx, |workspace, _| workspace.active_pane().clone());
    assert_eq!(pane.read_with(&window_cx, |pane, _| pane.items_len()), 1);
    pane.update_in(&mut window_cx, |pane, window, cx| {
        pane.close_active_item(&CloseActiveItem::default(), window, cx)
    })
    .await
    .unwrap();
    assert_eq!(task.await, json!({ "title": "Review", "excerpt_count": 1 }));
    window_cx.run_until_parked();
    assert!(focused_chat.get());
}

#[gpui::test]
async fn test_mcp_switches_workspace_repository_and_branch(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        "/",
        json!({
            "first": { ".git": {} },
            "second": { ".git": {} },
            "other": {},
        }),
    )
    .await;
    fs.insert_branches(Path::new("/first/.git"), &["main"]);
    fs.insert_branches(Path::new("/second/.git"), &["main", "feature"]);
    let project = Project::test(fs.clone(), [Path::new("/first"), Path::new("/second")], cx).await;
    let other = Project::test(fs, [Path::new("/other")], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let source_id = window
        .update(cx, |root, window, cx| {
            let source = root.workspace().clone();
            let source_id = source.update(cx, |workspace, _| {
                workspace.set_random_database_id();
                i64::from(workspace.database_id().unwrap())
            });
            root.add(source, window, cx);
            root.test_add_workspace(other, window, cx);
            source_id
        })
        .unwrap();
    let mut async_cx = cx.to_async();
    let listed = call(ListWorkspaces, json!({}), &mut async_cx).await;
    assert!(
        listed["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|workspace| {
                workspace["workspace_id"] == source_id && workspace["active"] == false
            })
    );
    let workspace = call(
        SwitchWorkspace,
        json!({ "workspace_id": source_id }),
        &mut async_cx,
    )
    .await;
    assert_eq!(workspace["active"], true);
    let repositories = call(
        ListRepositories,
        json!({ "workspace_id": source_id }),
        &mut async_cx,
    )
    .await;
    assert_eq!(repositories["repositories"].as_array().unwrap().len(), 2);
    let repository = call(
        SwitchRepository,
        json!({ "workspace_id": source_id, "path": "/second" }),
        &mut async_cx,
    )
    .await;
    assert_eq!(repository["active"], true);
    let branch = call(
        SwitchBranch,
        json!({ "workspace_id": source_id, "branch": "feature" }),
        &mut async_cx,
    )
    .await;
    assert_eq!(branch, json!({ "branch": "feature", "path": "/second" }));
    let branches = call(
        ListBranches,
        json!({ "workspace_id": source_id }),
        &mut async_cx,
    )
    .await;
    assert!(
        branches["branches"]
            .as_array()
            .unwrap()
            .contains(&json!({ "name": "feature", "current": true }))
    );
}
