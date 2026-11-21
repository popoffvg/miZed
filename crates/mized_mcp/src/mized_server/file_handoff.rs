use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use anyhow::{Context as _, Result, ensure};
use context_server::listener::{McpServerTool, ToolResponse};
use editor::{Editor, MultiBuffer, PathKey};
use futures::{StreamExt as _, channel::oneshot, stream};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, IntoElement, Render, SharedString, Window,
};
use language::{Buffer, Point};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workspace::{OpenOptions, Workspace, item::Item};

use super::{WorkspaceTarget, mcp_response, mized_workspace};

#[derive(Clone)]
pub(super) struct OpenFile;

#[derive(Clone)]
pub(super) struct OpenDraft;

#[derive(Clone)]
pub(super) struct OpenMultibuffer;

/// Open an existing local file in miZed.
#[derive(Deserialize, JsonSchema)]
pub(super) struct OpenFileInput {
    #[serde(flatten)]
    workspace: WorkspaceTarget,
    path: PathBuf,
    /// Wait until the user closes the file tab.
    wait_for_close: Option<bool>,
}

/// Open Markdown text in miZed and return the edited text when the user closes the tab.
#[derive(Deserialize, JsonSchema)]
pub(super) struct OpenDraftInput {
    #[serde(flatten)]
    workspace: WorkspaceTarget,
    title: String,
    text: String,
}

/// Open excerpts in one miZed editor tab. Group excerpts from the same file.
#[derive(Deserialize, JsonSchema)]
pub(super) struct OpenMultibufferInput {
    #[serde(flatten)]
    workspace: WorkspaceTarget,
    title: String,
    excerpts: Vec<MultibufferExcerpt>,
}

#[derive(Deserialize, JsonSchema)]
struct MultibufferExcerpt {
    path: PathBuf,
    /// First line, starting at 1.
    start_line: u32,
    /// Inclusive last line, starting at 1.
    end_line: u32,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct OpenedFile {
    path: PathBuf,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct EditedDraft {
    text: String,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct OpenedMultibuffer {
    title: String,
    excerpt_count: usize,
}

impl McpServerTool for OpenFile {
    type Input = OpenFileInput;
    type Output = OpenedFile;
    const NAME: &'static str = "open_file";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        ensure!(
            input.path.is_absolute(),
            "Use an absolute path to open a file in miZed"
        );
        let (window, workspace) =
            cx.update(|cx| mized_workspace(input.workspace.workspace_id, cx))?;
        let fs = cx.update(|cx| workspace.read(cx).app_state().fs.clone());
        let metadata = fs.metadata(&input.path).await?;
        ensure!(
            metadata.is_some_and(|metadata| !metadata.is_dir),
            "The miZed file does not exist: {}",
            input.path.display()
        );
        let task = window.update(cx, |root, window, cx| {
            root.activate(workspace.clone(), None, window, cx);
            workspace.update(cx, |workspace, cx| {
                workspace.open_abs_path(input.path.clone(), OpenOptions::default(), window, cx)
            })
        })?;
        let item = task.await?;
        if input.wait_for_close == Some(true) {
            let item_id = item.item_id();
            drop(item);
            wait_for_tab_close(workspace, item_id, cx).await?;
        }
        mcp_response(OpenedFile { path: input.path })
    }
}

impl McpServerTool for OpenMultibuffer {
    type Input = OpenMultibufferInput;
    type Output = OpenedMultibuffer;
    const NAME: &'static str = "open_multibuffer";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        ensure!(
            !input.title.trim().is_empty(),
            "Enter a title for the miZed multibuffer"
        );
        ensure!(
            !input.excerpts.is_empty(),
            "Provide at least one miZed multibuffer excerpt"
        );
        let (window, workspace) =
            cx.update(|cx| mized_workspace(input.workspace.workspace_id, cx))?;
        let (project, fs) = workspace.read_with(cx, |workspace, _| {
            (
                workspace.project().clone(),
                workspace.app_state().fs.clone(),
            )
        });
        for excerpt in &input.excerpts {
            ensure!(
                excerpt.path.is_absolute(),
                "Use an absolute path for each miZed excerpt"
            );
            ensure!(
                excerpt.start_line > 0 && excerpt.start_line <= excerpt.end_line,
                "Use an inclusive line range that starts at 1 or later"
            );
        }
        let mut paths = HashSet::new();
        let mut buffers = HashMap::new();
        {
            let paths = input
                .excerpts
                .iter()
                .map(|excerpt| &excerpt.path)
                .filter(|path| paths.insert(*path));
            let checks = stream::iter(paths)
                .map(|path| {
                    let fs = &fs;
                    let project = &project;
                    let mut cx = cx.clone();
                    async move {
                        ensure!(
                            fs.metadata(path)
                                .await?
                                .is_some_and(|metadata| !metadata.is_dir),
                            "The miZed excerpt file does not exist: {}",
                            path.display()
                        );
                        let buffer = project
                            .update(&mut cx, |project, cx| project.open_local_buffer(path, cx))
                            .await?;
                        anyhow::Ok((path.clone(), buffer))
                    }
                })
                .buffered(8);
            futures::pin_mut!(checks);
            while let Some(result) = checks.next().await {
                let (path, buffer) = result?;
                buffers.insert(path, buffer);
            }
        }
        let mut excerpts = HashMap::new();
        for (index, excerpt) in input.excerpts.into_iter().enumerate() {
            let buffer = buffers
                .get(&excerpt.path)
                .context("The miZed excerpt file did not open")?;
            let range = buffer.read_with(cx, |buffer, _| {
                let start = excerpt.start_line - 1;
                let end = excerpt.end_line - 1;
                ensure!(
                    end <= buffer.max_point().row,
                    "The miZed excerpt ends past the file: {}",
                    excerpt.path.display(),
                );
                Ok::<_, anyhow::Error>(Point::new(start, 0)..Point::new(end, buffer.line_len(end)))
            })?;
            excerpts
                .entry(buffer.clone())
                .or_insert_with(|| (PathKey::sorted(index as u64), Vec::new()))
                .1
                .push(range);
        }

        let title = input.title;
        let excerpt_count = window.update(cx, |root, window, cx| {
            root.activate(workspace.clone(), None, window, cx);
            workspace.update(cx, |workspace, cx| {
                let capability = project.read(cx).capability();
                let multibuffer = cx.new(|cx| {
                    let mut multibuffer = MultiBuffer::new(capability).with_title(title.clone());
                    for (buffer, (path, ranges)) in excerpts {
                        multibuffer.set_excerpts_for_path(path, buffer, ranges, 0, cx);
                    }
                    multibuffer
                });
                let excerpt_count = multibuffer.read(cx).snapshot(cx).excerpts().count();
                let editor =
                    cx.new(|cx| Editor::for_multibuffer(multibuffer, Some(project), window, cx));
                workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
                excerpt_count
            })
        })?;
        mcp_response(OpenedMultibuffer {
            title,
            excerpt_count,
        })
    }
}

impl McpServerTool for OpenDraft {
    type Input = OpenDraftInput;
    type Output = EditedDraft;
    const NAME: &'static str = "open_draft";

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        ensure!(
            !input.title.trim().is_empty(),
            "Enter a title for the miZed draft"
        );
        let (window, workspace) =
            cx.update(|cx| mized_workspace(input.workspace.workspace_id, cx))?;
        let languages = cx.update(|cx| workspace.read(cx).app_state().languages.clone());
        let markdown = languages.language_for_name("Markdown").await?;
        let (item_id, buffer) = window.update(cx, |root, window, cx| {
            root.activate(workspace.clone(), None, window, cx);
            let buffer = cx.new(|cx| {
                let mut buffer = Buffer::local(input.text, cx);
                buffer.set_language(Some(markdown), cx);
                buffer
            });
            let draft = cx.new(|cx| {
                let editor = cx.new(|cx| Editor::for_buffer(buffer.clone(), None, window, cx));
                DraftTab {
                    title: input.title.into(),
                    editor,
                }
            });
            let item_id = draft.entity_id();
            workspace.update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(draft), None, true, window, cx);
            });
            (item_id, buffer)
        })?;
        wait_for_tab_close(workspace, item_id, cx).await?;
        let text = buffer.read_with(cx, |buffer, _| buffer.text());
        mcp_response(EditedDraft { text })
    }
}

async fn wait_for_tab_close(
    workspace: Entity<Workspace>,
    item_id: EntityId,
    cx: &mut AsyncApp,
) -> Result<()> {
    let (sender, receiver) = oneshot::channel();
    let subscription = cx.update(|cx| {
        if !workspace.read(cx).items(cx).any(|item| item.item_id() == item_id) {
            return None;
        }
        let mut sender = Some(sender);
        Some(cx.subscribe(&workspace, move |workspace, event, cx| {
            if matches!(event, workspace::Event::ItemRemoved { item_id: removed } if *removed == item_id)
                && !workspace.read(cx).items(cx).any(|item| item.item_id() == item_id)
                && let Some(sender) = sender.take()
            {
                sender.send(()).ok();
            }
        }))
    });
    drop(workspace);
    if let Some(_subscription) = subscription {
        receiver
            .await
            .context("miZed dropped the workspace before the tab closed")?;
    }
    Ok(())
}

struct DraftTab {
    title: SharedString,
    editor: Entity<Editor>,
}

impl Render for DraftTab {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.editor.clone()
    }
}

impl Focusable for DraftTab {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl EventEmitter<()> for DraftTab {}

impl Item for DraftTab {
    type Event = ();

    fn tab_content_text(&self, _: usize, _: &App) -> SharedString {
        self.title.clone()
    }
}
