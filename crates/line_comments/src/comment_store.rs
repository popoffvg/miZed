use std::{collections::BTreeMap, path::Path, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context as _;
use collections::HashMap;
use fs::Fs;
use futures::{StreamExt as _, channel::oneshot};
use gpui::{App, AppContext as _, Context, Entity, Global, Task, WeakEntity};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;
use workspace::WorkspaceId;

use crate::comment_store_path::{CommentStoreName, line_comment_store_path};

pub(crate) const STORE_VERSION: u32 = 1;
const STORE_WATCH_LATENCY: Duration = Duration::from_millis(100);

/// Comments in one named workspace store.
pub struct CommentStore {
    files: BTreeMap<String, Vec<LineComment>>,
    loaded: bool,
    revision: u64,
    completed_revision: u64,
    saves: async_channel::Sender<CommentSave>,
    _watch: Task<()>,
}

struct CommentSave {
    store: Entity<CommentStore>,
    revision: u64,
    change: CommentChange,
    reply: oneshot::Sender<anyhow::Result<CommentSaveResult>>,
}

enum CommentChange {
    Edit(CommentEdit),
    Patch {
        path: String,
        line: u32,
        text: Option<String>,
        done: Option<bool>,
    },
    Clear {
        path: Option<String>,
    },
}

#[derive(Default)]
struct CommentSaveResult {
    cleared: usize,
    edited: Option<LineComment>,
}

struct CommentEdit {
    path: String,
    line: u32,
    previous: Option<LineComment>,
    replacement: Option<LineComment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineComment {
    /// 1-based line that the comment starts on.
    pub line: u32,
    /// Optional 1-based last line of the comment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
    #[serde(default)]
    pub orphaned: bool,
    #[serde(default)]
    pub author: Author,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Author {
    #[default]
    Human,
    Agent,
}

#[derive(Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    files: BTreeMap<String, Vec<LineComment>>,
}

#[derive(Default)]
struct CommentStores(HashMap<(WorkspaceId, CommentStoreName), WeakEntity<CommentStore>>);

impl Global for CommentStores {}

impl LineComment {
    pub fn last_line(&self) -> u32 {
        self.end_line.unwrap_or(self.line).max(self.line)
    }
}

impl CommentStore {
    /// Editors that select the same workspace and name share one store and watch.
    pub fn workspace_comment_store(
        workspace_id: WorkspaceId,
        name: CommentStoreName,
        fs: Arc<dyn Fs>,
        cx: &mut App,
    ) -> Entity<CommentStore> {
        let key = (workspace_id, name);
        if let Some(store) = cx
            .default_global::<CommentStores>()
            .0
            .get(&key)
            .and_then(WeakEntity::upgrade)
        {
            return store;
        }
        let path = line_comment_store_path(workspace_id.into(), &key.1);
        let (saves, pending_saves) = async_channel::unbounded();
        let save_path = path.clone();
        let save_fs = fs.clone();
        cx.spawn(async move |cx| {
            let mut recreate_folder = false;
            while let Ok(CommentSave {
                store,
                revision,
                change,
                reply,
            }) = pending_saves.recv().await
            {
                let path = save_path.clone();
                let fs = save_fs.clone();
                let (result, files) = cx
                    .background_spawn(async move {
                        match write_comment_change(fs.as_ref(), &path, change, recreate_folder)
                            .await
                        {
                            Ok((files, result)) => (Ok(result), Some(files)),
                            Err(error) => (Err(error), load_store(fs.as_ref(), &path).await.ok()),
                        }
                    })
                    .await;
                recreate_folder = result.is_err();
                store.update(cx, |store, cx| {
                    store.completed_revision = revision;
                    if let Some(files) = files {
                        store.files = files;
                        cx.notify();
                    }
                });
                reply.send(result).ok();
            }
        })
        .detach();
        let store = cx.new(|cx| CommentStore {
            files: BTreeMap::new(),
            loaded: false,
            revision: 0,
            completed_revision: 0,
            saves,
            _watch: cx.spawn(async move |store, cx| watch_store(store, path, fs, cx).await),
        });
        cx.default_global::<CommentStores>()
            .0
            .insert(key, store.downgrade());
        store
    }

    /// The comments stored for a file path.
    pub fn comments(&self, path: &str) -> &[LineComment] {
        self.files.get(path).map(Vec::as_slice).unwrap_or_default()
    }

    pub fn all_comments(&self) -> impl Iterator<Item = (&str, &LineComment)> {
        self.files.iter().flat_map(|(path, comments)| {
            comments.iter().map(move |comment| (path.as_str(), comment))
        })
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn add_comment(
        &mut self,
        path: &str,
        comment: LineComment,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        if self
            .comments(path)
            .iter()
            .any(|existing| existing.line == comment.line)
        {
            return Task::ready(Err(anyhow::anyhow!(
                "A comment already starts on this line"
            )));
        }
        let line = comment.line;
        self.save_change(path, line, None, Some(comment), cx)
    }

    /// Replace the comment that starts on the same line, or add it, and save the store.
    pub fn upsert(
        &mut self,
        path: &str,
        comment: LineComment,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let line = comment.line;
        let previous = self
            .comments(path)
            .iter()
            .find(|existing| existing.line == line)
            .cloned();
        self.save_change(path, line, previous, Some(comment), cx)
    }

    pub(crate) fn set_comment_done(
        &mut self,
        path: &str,
        line: u32,
        done: bool,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let save = self.edit_comment(path, line, None, Some(done), cx);
        cx.spawn(async move |_, _| save.await.map(drop))
    }

    pub fn edit_comment(
        &mut self,
        path: &str,
        line: u32,
        text: Option<String>,
        done: Option<bool>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<LineComment>> {
        if !Path::new(path).is_absolute() {
            return Task::ready(Err(anyhow::anyhow!(
                "Edit line comments with an absolute file path"
            )));
        }
        self.save(
            CommentChange::Patch {
                path: path.to_owned(),
                line,
                text,
                done,
            },
            |result| result.edited.context("The line comment edit has no result"),
            cx,
        )
    }

    /// Remove the comment that starts on this line, and save the store.
    pub fn remove(
        &mut self,
        path: &str,
        line: u32,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let previous = self
            .comments(path)
            .iter()
            .find(|comment| comment.line == line)
            .cloned();
        self.save_change(path, line, previous, None, cx)
    }

    /// Clear comments in one file or in the entire store.
    pub fn clear_comments(
        &mut self,
        path: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<usize>> {
        if path.is_some_and(|path| !Path::new(path).is_absolute()) {
            return Task::ready(Err(anyhow::anyhow!(
                "Clear line comments with an absolute file path"
            )));
        }
        self.save(
            CommentChange::Clear {
                path: path.map(str::to_owned),
            },
            |result| Ok(result.cleared),
            cx,
        )
    }

    fn save_change(
        &mut self,
        path: &str,
        line: u32,
        previous: Option<LineComment>,
        replacement: Option<LineComment>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        if !Path::new(path).is_absolute() {
            return Task::ready(Err(anyhow::anyhow!(
                "Save line comments with an absolute file path"
            )));
        }
        self.save(
            CommentChange::Edit(CommentEdit {
                path: path.to_owned(),
                line,
                previous,
                replacement,
            }),
            |_| Ok(()),
            cx,
        )
    }

    fn save<T: 'static>(
        &mut self,
        change: CommentChange,
        output: impl FnOnce(CommentSaveResult) -> anyhow::Result<T> + 'static,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<T>> {
        let revision = self.revision + 1;
        let (reply, result) = oneshot::channel();
        if self
            .saves
            .try_send(CommentSave {
                store: cx.entity(),
                revision,
                change,
                reply,
            })
            .is_err()
        {
            return Task::ready(Err(anyhow::anyhow!(
                "The line comment save queue is closed"
            )));
        }
        self.revision = revision;
        cx.spawn(async move |_, _| {
            let result = result
                .await
                .context("The line comment save did not complete")??;
            output(result)
        })
    }
}

async fn write_comment_change(
    fs: &dyn Fs,
    path: &Path,
    change: CommentChange,
    recreate_folder: bool,
) -> anyhow::Result<(BTreeMap<String, Vec<LineComment>>, CommentSaveResult)> {
    if recreate_folder && let Some(folder) = path.parent() {
        fs.create_dir(folder).await?;
    }
    let previous_files = load_store(fs, path).await?;
    let mut files = previous_files.clone();
    let mut result = CommentSaveResult::default();
    match change {
        CommentChange::Edit(edit) => {
            apply_comment_edit(&mut files, edit)?;
        }
        CommentChange::Patch {
            path,
            line,
            text,
            done,
        } => {
            let comment = files
                .get_mut(&path)
                .and_then(|comments| comments.iter_mut().find(|comment| comment.line == line))
                .context("The line comment no longer exists")?;
            if let Some(text) = text {
                comment.text = text;
            }
            if let Some(done) = done {
                comment.done = done;
            }
            result.edited = Some(comment.clone());
        }
        CommentChange::Clear { path } => {
            result.cleared = if let Some(path) = path {
                files.remove(&path).map_or(0, |comments| comments.len())
            } else {
                let cleared = files.values().map(Vec::len).sum();
                files.clear();
                cleared
            };
        }
    }
    if files == previous_files {
        return Ok((files, result));
    }
    let store_file = StoreFile {
        version: STORE_VERSION,
        files,
    };
    let text = serde_json::to_string_pretty(&store_file)?;
    anyhow::ensure!(
        load_store(fs, path).await? == previous_files,
        "The line comment store changed during the save"
    );
    fs.atomic_write(path.to_path_buf(), text).await?;
    Ok((store_file.files, result))
}

fn apply_comment_edit(
    files: &mut BTreeMap<String, Vec<LineComment>>,
    change: CommentEdit,
) -> anyhow::Result<()> {
    let comments = files.entry(change.path.clone()).or_default();
    let current = comments.iter().find(|comment| comment.line == change.line);
    anyhow::ensure!(
        current == change.previous.as_ref(),
        "The line comment on disk changed before it could be saved"
    );
    comments.retain(|comment| comment.line != change.line);
    if let Some(replacement) = change.replacement {
        comments.push(replacement);
        comments.sort_by_key(|comment| comment.line);
    }
    if comments.is_empty() {
        files.remove(&change.path);
    }
    Ok(())
}

async fn watch_store(
    store: WeakEntity<CommentStore>,
    path: PathBuf,
    fs: Arc<dyn Fs>,
    cx: &mut gpui::AsyncApp,
) {
    let Some(folder) = path.parent() else {
        return;
    };
    'watch: loop {
        fs.create_dir(folder).await.log_err();
        let (mut events, _watcher) = fs.watch(folder, STORE_WATCH_LATENCY).await;
        loop {
            let Ok(revision) = store.read_with(cx, |store, _| {
                (store.revision == store.completed_revision).then_some(store.revision)
            }) else {
                return;
            };
            let Some(revision) = revision else {
                cx.background_executor().timer(STORE_WATCH_LATENCY).await;
                continue;
            };
            let files = load_store(fs.as_ref(), &path).await.log_err();
            let Ok(applied) = store.update(cx, |store, cx| {
                if revision != store.revision {
                    return false;
                }
                store.loaded = files.is_some();
                if let Some(files) = files {
                    store.files = files;
                }
                cx.notify();
                true
            }) else {
                return;
            };
            if !applied {
                continue;
            }
            loop {
                let Some(events) = events.next().await else {
                    return;
                };
                if events.iter().any(|event| event.path == folder) {
                    continue 'watch;
                }
                if events.iter().any(|event| event.path == path) {
                    break;
                }
            }
        }
    }
}

async fn load_store(
    fs: &dyn Fs,
    path: &Path,
) -> anyhow::Result<BTreeMap<String, Vec<LineComment>>> {
    let text = match fs.load(path).await {
        Ok(text) => text,
        Err(_) if fs.metadata(path).await?.is_none() => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    if text.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let store: StoreFile = serde_json::from_str(&text)
        .with_context(|| format!("Could not read the line comments in {}", path.display()))?;
    anyhow::ensure!(
        store.version == STORE_VERSION,
        "The line comment store {} has version {}, and miZed reads version {STORE_VERSION}",
        path.display(),
        store.version
    );
    Ok(store.files)
}
