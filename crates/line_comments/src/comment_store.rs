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

/// Comments in one named store, keyed by absolute file path.
pub struct CommentStore {
    files: BTreeMap<String, Vec<LineComment>>,
    loaded: bool,
    revision: u64,
    saved_revision: u64,
    saves: async_channel::Sender<CommentSave>,
    _watch: Task<()>,
}

struct CommentSave {
    store: Entity<CommentStore>,
    revision: u64,
    files: BTreeMap<String, Vec<LineComment>>,
    reply: oneshot::Sender<anyhow::Result<()>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineComment {
    /// 1-based line that the comment starts on.
    pub line: u32,
    /// 1-based last line, absent when the comment covers one line.
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
                files,
                reply,
            }) = pending_saves.recv().await
            {
                let path = save_path.clone();
                let fs = save_fs.clone();
                let result = cx
                    .background_spawn(async move {
                        if recreate_folder && let Some(folder) = path.parent() {
                            fs.create_dir(folder).await?;
                        }
                        let text = serde_json::to_string_pretty(&StoreFile {
                            version: STORE_VERSION,
                            files,
                        })?;
                        fs.atomic_write(path, text).await
                    })
                    .await;
                recreate_folder = result.is_err();
                if result.is_ok() {
                    store.update(cx, |store, _| store.saved_revision = revision);
                }
                reply.send(result).ok();
            }
        })
        .detach();
        let store = cx.new(|cx| CommentStore {
            files: BTreeMap::new(),
            loaded: false,
            revision: 0,
            saved_revision: 0,
            saves,
            _watch: cx.spawn(async move |store, cx| watch_store(store, path, fs, cx).await),
        });
        cx.default_global::<CommentStores>()
            .0
            .insert(key, store.downgrade());
        store
    }

    /// The comments of one file, by its absolute path.
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
        self.upsert(path, comment, cx)
    }

    /// Replace the comment that starts on the same line, or add it, and save the store.
    pub fn upsert(
        &mut self,
        path: &str,
        comment: LineComment,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let comments = self.files.entry(path.to_string()).or_default();
        comments.retain(|existing| existing.line != comment.line);
        comments.push(comment);
        comments.sort_by_key(|comment| comment.line);
        self.save(cx)
    }

    pub(crate) fn set_comment_done(
        &mut self,
        path: &str,
        line: u32,
        done: bool,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        let Some(comment) = self
            .files
            .get_mut(path)
            .and_then(|comments| comments.iter_mut().find(|comment| comment.line == line))
        else {
            return Task::ready(Err(anyhow::anyhow!("The line comment no longer exists")));
        };
        if comment.done == done && self.revision == self.saved_revision {
            return Task::ready(Ok(()));
        }
        comment.done = done;
        self.save(cx)
    }

    /// Remove the comment that starts on this line, and save the store.
    pub fn remove(
        &mut self,
        path: &str,
        line: u32,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<()>> {
        if let Some(comments) = self.files.get_mut(path) {
            comments.retain(|comment| comment.line != line);
            if comments.is_empty() {
                self.files.remove(path);
            }
        }
        self.save(cx)
    }

    fn save(&mut self, cx: &mut Context<Self>) -> Task<anyhow::Result<()>> {
        cx.notify();
        self.revision += 1;
        let (reply, result) = oneshot::channel();
        if self
            .saves
            .try_send(CommentSave {
                store: cx.entity(),
                revision: self.revision,
                files: self.files.clone(),
                reply,
            })
            .is_err()
        {
            return Task::ready(Err(anyhow::anyhow!(
                "The line comment save queue is closed"
            )));
        }
        cx.spawn(async move |_, _| {
            result
                .await
                .context("The line comment save did not complete")?
        })
    }
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
    fs.create_dir(folder).await.log_err();
    let (mut events, _watcher) = fs.watch(folder, STORE_WATCH_LATENCY).await;
    loop {
        let Ok(revision) = store.read_with(cx, |store, _| {
            (store.revision == store.saved_revision).then_some(store.revision)
        }) else {
            return;
        };
        let files = load_store(fs.as_ref(), &path).await.log_err();
        if store
            .update(cx, |store, cx| {
                if revision != Some(store.revision) {
                    return;
                }
                store.loaded = files.is_some();
                if let Some(files) = files {
                    store.files = files;
                }
                cx.notify();
            })
            .is_err()
        {
            return;
        }
        loop {
            let Some(events) = events.next().await else {
                return;
            };
            if events.iter().any(|event| event.path == path) {
                break;
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
