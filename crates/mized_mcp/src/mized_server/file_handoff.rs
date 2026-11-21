use std::{collections::HashMap, path::PathBuf};

use anyhow::{Context as _, Result, ensure};
use context_server::listener::{McpServerTool, ToolResponse};
use editor::{Editor, MultiBuffer, PathKey};
use futures::channel::oneshot;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Render, SharedString, Window,
};
use language::{Buffer, Point};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workspace::{OpenOptions, item::Item};

use super::{mcp_response, mized_workspace};

#[derive(Clone)]
pub(super) struct OpenFile;

#[derive(Clone)]
pub(super) struct OpenDraft;

#[derive(Clone)]
pub(super) struct OpenMultibuffer;

/// Open an existing local file in miZed.
#[derive(Deserialize, JsonSchema)]
pub(super) struct OpenFileInput {
    /// ID of an open miZed workspace. Defaults to the active local workspace.
    workspace_id: Option<i64>,
    path: PathBuf,
    /// Wait until the user closes the file tab.
    wait_for_close: Option<bool>,
}

/// Open Markdown text in miZed and return the edited text when the user closes the tab.
#[derive(Deserialize, JsonSchema)]
pub(super) struct OpenDraftInput {
    /// ID of an open miZed workspace. Defaults to the active local workspace.
    workspace_id: Option<i64>,
    title: String,
    text: String,
}

/// Open excerpts from local files in one miZed editor tab.
#[derive(Deserialize, JsonSchema)]
pub(super) struct OpenMultibufferInput {
    /// ID of an open miZed workspace. Defaults to the active local workspace.
    workspace_id: Option<i64>,
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
        let (window, workspace) = cx.update(|cx| mized_workspace(input.workspace_id, cx))?;
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
            let (sender, receiver) = oneshot::channel();
            let _subscription = cx.update(|cx| {
                let subscription = item.on_release(
                    cx,
                    Box::new(move |_| {
                        sender.send(()).ok();
                    }),
                );
                drop(item);
                subscription
            });
            receiver
                .await
                .context("miZed did not report that the file closed")?;
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
        let (window, workspace) = cx.update(|cx| mized_workspace(input.workspace_id, cx))?;
        let (project, fs) = workspace.read_with(cx, |workspace, _| {
            (
                workspace.project().clone(),
                workspace.app_state().fs.clone(),
            )
        });
        let mut excerpts = HashMap::new();
        for (index, excerpt) in input.excerpts.into_iter().enumerate() {
            ensure!(
                excerpt.path.is_absolute(),
                "Use an absolute path for each miZed excerpt"
            );
            ensure!(
                excerpt.start_line > 0 && excerpt.start_line <= excerpt.end_line,
                "Use an inclusive line range that starts at 1 or later"
            );
            ensure!(
                fs.metadata(&excerpt.path)
                    .await?
                    .is_some_and(|metadata| !metadata.is_dir),
                "The miZed excerpt file does not exist: {}",
                excerpt.path.display()
            );
            let buffer = project
                .update(cx, |project, cx| {
                    project.open_local_buffer(&excerpt.path, cx)
                })
                .await?;
            let range = buffer.read_with(cx, |buffer, _| {
                let start = excerpt.start_line - 1;
                let end = excerpt.end_line - 1;
                ensure!(
                    end <= buffer.max_point().row,
                    "The miZed excerpt ends past the file: {}",
                    excerpt.path.display()
                );
                Ok::<_, anyhow::Error>(Point::new(start, 0)..Point::new(end, buffer.line_len(end)))
            })?;
            excerpts
                .entry(buffer)
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
        let (window, workspace) = cx.update(|cx| mized_workspace(input.workspace_id, cx))?;
        let languages = cx.update(|cx| workspace.read(cx).app_state().languages.clone());
        let markdown = languages.language_for_name("Markdown").await?;
        let (sender, receiver) = oneshot::channel();
        window.update(cx, |root, window, cx| {
            root.activate(workspace.clone(), None, window, cx);
            let buffer = cx.new(|cx| {
                let mut buffer = Buffer::local(input.text, cx);
                buffer.set_language(Some(markdown), cx);
                buffer
            });
            let draft = cx.new(|cx| {
                let editor = cx.new(|cx| Editor::for_buffer(buffer.clone(), None, window, cx));
                cx.on_release(move |_, cx| {
                    sender.send(buffer.read(cx).text()).ok();
                })
                .detach();
                DraftTab {
                    title: input.title.into(),
                    editor,
                }
            });
            workspace.update(cx, |workspace, cx| {
                workspace.add_item_to_active_pane(Box::new(draft), None, true, window, cx);
            });
        })?;
        let text = receiver
            .await
            .context("miZed did not return the draft text")?;
        mcp_response(EditedDraft { text })
    }
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
