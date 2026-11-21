use std::{path::Path, rc::Rc, time::Duration};

use anyhow::{Context as _, Result, ensure};
use context_server::{
    listener::{McpServerTool, ToolResponse},
    types::ToolAnnotations,
};
use futures::{FutureExt as _, channel::oneshot, lock::Mutex};
use gpui::{AsyncApp, Entity};
use line_comments::{Author, CommentStore, CommentStoreName, LineComment};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{mcp_response, mized_workspace};

#[derive(Clone, Default)]
pub(super) struct AddComment(Rc<Mutex<()>>);

#[derive(Clone)]
pub(super) struct ReadComment;

#[derive(Clone)]
pub(super) struct ListComments;

#[derive(Deserialize, JsonSchema)]
struct CommentStoreTarget {
    /// ID of an open miZed workspace. Defaults to the active local workspace.
    workspace_id: Option<i64>,
    /// Name of the workspace comment store. Defaults to "default".
    store: Option<String>,
}

/// Add an agent line comment to a named miZed workspace store.
#[derive(Deserialize, JsonSchema)]
pub(super) struct AddCommentInput {
    #[serde(flatten)]
    store: CommentStoreTarget,
    /// Absolute file path. Use the file path shown in the editor or diff.
    path: String,
    /// First line in the working tree, starting at 1.
    line: u32,
    /// Inclusive last line, starting at 1.
    end_line: Option<u32>,
    text: String,
}

/// Read the line comment that starts on a line in a named miZed workspace store.
#[derive(Deserialize, JsonSchema)]
pub(super) struct ReadCommentInput {
    #[serde(flatten)]
    store: CommentStoreTarget,
    /// Absolute file path.
    path: String,
    /// The first line of the comment, starting at 1.
    line: u32,
}

/// List line comments in a named miZed workspace store.
#[derive(Deserialize, JsonSchema)]
pub(super) struct ListCommentsInput {
    #[serde(flatten)]
    store: CommentStoreTarget,
    /// Optional absolute file path to filter the list.
    path: Option<String>,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct CommentResult {
    path: String,
    comment: Option<serde_json::Value>,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct CommentList {
    comments: Vec<CommentResult>,
}

impl McpServerTool for AddComment {
    type Input = AddCommentInput;
    type Output = CommentResult;
    const NAME: &'static str = "add_comment";

    fn annotations(&self) -> ToolAnnotations {
        comment_annotations(false)
    }

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        check_comment_path(&input.path)?;
        ensure!(input.line > 0, "The first comment line must be at least 1");
        ensure!(
            input.end_line.is_none_or(|end| end >= input.line),
            "The last comment line must follow the first"
        );
        let text = input.text.trim().to_owned();
        ensure!(!text.is_empty(), "Enter text for the line comment");
        let comment = LineComment {
            line: input.line,
            end_line: input.end_line.filter(|end| *end > input.line),
            text,
            done: false,
            orphaned: false,
            author: Author::Agent,
        };

        let _write = self.0.lock().await;
        let store = open_comment_store(input.store, cx).await?;
        let save = cx.update(|cx| {
            store.update(cx, |store, cx| {
                ensure!(
                    !store
                        .comments(&input.path)
                        .iter()
                        .any(|existing| existing.line == input.line),
                    "A comment already starts on this line"
                );
                Ok(store.upsert(&input.path, comment.clone(), cx))
            })
        })?;
        save.await?;
        mcp_response(comment_result(&input.path, Some(&comment))?)
    }
}

impl McpServerTool for ReadComment {
    type Input = ReadCommentInput;
    type Output = CommentResult;
    const NAME: &'static str = "read_comment";

    fn annotations(&self) -> ToolAnnotations {
        comment_annotations(true)
    }

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        check_comment_path(&input.path)?;
        ensure!(input.line > 0, "The first comment line must be at least 1");
        let store = open_comment_store(input.store, cx).await?;
        let result = cx.update(|cx| {
            let comment = store
                .read(cx)
                .comments(&input.path)
                .iter()
                .find(|comment| comment.line == input.line);
            comment_result(&input.path, comment)
        })?;
        mcp_response(result)
    }
}

impl McpServerTool for ListComments {
    type Input = ListCommentsInput;
    type Output = CommentList;
    const NAME: &'static str = "list_comments";

    fn annotations(&self) -> ToolAnnotations {
        comment_annotations(true)
    }

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        if let Some(path) = &input.path {
            check_comment_path(path)?;
        }
        let store = open_comment_store(input.store, cx).await?;
        let comments = cx.update(|cx| {
            store
                .read(cx)
                .all_comments()
                .filter(|(path, _)| input.path.as_deref().is_none_or(|filter| *path == filter))
                .map(|(path, comment)| comment_result(path, Some(comment)))
                .collect::<Result<Vec<_>>>()
        })?;
        mcp_response(CommentList { comments })
    }
}

async fn open_comment_store(
    target: CommentStoreTarget,
    cx: &mut AsyncApp,
) -> Result<Entity<CommentStore>> {
    let name: CommentStoreName = target
        .store
        .as_deref()
        .map(str::parse)
        .transpose()?
        .unwrap_or_default();
    let (store, readiness) = cx.update(|cx| -> Result<_> {
        let (_, workspace) = mized_workspace(target.workspace_id, cx)?;
        let workspace = workspace.read(cx);
        let id = workspace
            .database_id()
            .context("Line comments need a saved workspace")?;
        let fs = workspace.app_state().fs.clone();
        let store = CommentStore::workspace_comment_store(id, name, fs, cx);
        let readiness = if store.read(cx).is_loaded() {
            None
        } else {
            let (sender, receiver) = oneshot::channel();
            let mut sender = Some(sender);
            let subscription = cx.observe(&store, move |store, cx| {
                if store.read(cx).is_loaded()
                    && let Some(sender) = sender.take()
                {
                    sender.send(()).ok();
                }
            });
            Some((subscription, receiver))
        };
        Ok((store, readiness))
    })?;
    if let Some((_subscription, receiver)) = readiness {
        futures::select! {
            result = receiver.fuse() => result.context("The miZed comment store closed before it loaded")?,
            _ = cx.background_executor().timer(Duration::from_secs(5)).fuse() => {
                anyhow::bail!("The miZed comment store did not load");
            }
        }
    }
    Ok(store)
}

fn check_comment_path(path: &str) -> Result<()> {
    ensure!(
        Path::new(path).is_absolute(),
        "Use an absolute file path for line comments"
    );
    Ok(())
}

fn comment_result(path: &str, comment: Option<&LineComment>) -> Result<CommentResult> {
    Ok(CommentResult {
        path: path.to_owned(),
        comment: comment.map(serde_json::to_value).transpose()?,
    })
}

fn comment_annotations(read_only: bool) -> ToolAnnotations {
    ToolAnnotations {
        title: None,
        read_only_hint: Some(read_only),
        destructive_hint: Some(false),
        idempotent_hint: Some(read_only),
        open_world_hint: Some(false),
    }
}
