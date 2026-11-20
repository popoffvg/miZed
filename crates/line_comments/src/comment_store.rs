use std::{collections::BTreeMap, path::Path, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context as _;
use collections::HashMap;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Context, Entity, Global, Task, WeakEntity};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;
use workspace::WorkspaceId;

const STORE_VERSION: u32 = 1;
const STORE_WATCH_LATENCY: Duration = Duration::from_millis(100);

/// The line comments of one workspace, kept in `<Zed data dir>/line_comments/<workspace id>.json`
/// and keyed by the absolute path of each file. Claude reads and writes the same file.
pub struct CommentStore {
    path: PathBuf,
    fs: Arc<dyn Fs>,
    files: BTreeMap<String, Vec<LineComment>>,
    _watch: Task<()>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineComment {
    /// 1-based line that the comment starts on.
    pub line: u32,
    /// 1-based last line, absent when the comment covers one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    pub text: String,
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
struct CommentStores(HashMap<WorkspaceId, WeakEntity<CommentStore>>);

impl Global for CommentStores {}

impl LineComment {
    pub fn last_line(&self) -> u32 {
        self.end_line.unwrap_or(self.line).max(self.line)
    }
}

impl CommentStore {
    /// The store of a workspace. Editors of one workspace share one store and one watch.
    pub fn for_workspace(
        workspace_id: WorkspaceId,
        fs: Arc<dyn Fs>,
        cx: &mut App,
    ) -> Entity<CommentStore> {
        if let Some(store) = cx
            .default_global::<CommentStores>()
            .0
            .get(&workspace_id)
            .and_then(WeakEntity::upgrade)
        {
            return store;
        }
        let path = paths::data_dir()
            .join("line_comments")
            .join(format!("{}.json", i64::from(workspace_id)));
        let store = cx.new(|cx| CommentStore {
            path: path.clone(),
            fs: fs.clone(),
            files: BTreeMap::new(),
            _watch: cx.spawn(async move |store, cx| watch_store(store, path, fs, cx).await),
        });
        cx.default_global::<CommentStores>()
            .0
            .insert(workspace_id, store.downgrade());
        store
    }

    /// The comments of one file, by its absolute path.
    pub fn comments(&self, path: &str) -> &[LineComment] {
        self.files.get(path).map(Vec::as_slice).unwrap_or_default()
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

    /// Remove the comment that starts on this line, and save the store.
    pub fn remove(&mut self, path: &str, line: u32, cx: &mut Context<Self>) -> Task<anyhow::Result<()>> {
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
        let text = serde_json::to_string_pretty(&StoreFile {
            version: STORE_VERSION,
            files: self.files.clone(),
        });
        let path = self.path.clone();
        let fs = self.fs.clone();
        cx.background_spawn(async move {
            if let Some(folder) = path.parent() {
                fs.create_dir(folder).await?;
            }
            fs.atomic_write(path, text?).await
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
        let files = load_store(fs.as_ref(), &path)
            .await
            .log_err()
            .unwrap_or_default();
        if store
            .update(cx, |store, cx| {
                store.files = files;
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
    let Ok(text) = fs.load(path).await else {
        return Ok(BTreeMap::new());
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
