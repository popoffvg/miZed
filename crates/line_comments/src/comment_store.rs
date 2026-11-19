use std::{collections::BTreeMap, path::Path, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context as _;
use collections::HashMap;
use fs::Fs;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Entity, Global, Task, WeakEntity};
use serde::Deserialize;
use util::ResultExt as _;

const STORE_VERSION: u32 = 1;
const STORE_WATCH_LATENCY: Duration = Duration::from_millis(100);

/// The line comments of one project root, read from `<root>/.tmp/line-comment.json`.
/// `line-comment-lsp` and its `comment` subcommand write the same file.
pub struct CommentStore {
    files: BTreeMap<String, Vec<LineComment>>,
    _watch: Task<()>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LineComment {
    /// 1-based line that the comment starts on.
    pub line: u32,
    /// 1-based last line, absent when the comment covers one line.
    #[serde(default)]
    pub end_line: Option<u32>,
    pub text: String,
    #[serde(default)]
    pub orphaned: bool,
    #[serde(default)]
    pub author: Author,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Author {
    #[default]
    Human,
    Agent,
}

#[derive(Deserialize)]
struct StoreFile {
    version: u32,
    files: BTreeMap<String, Vec<LineComment>>,
}

#[derive(Default)]
struct CommentStores(HashMap<PathBuf, WeakEntity<CommentStore>>);

impl Global for CommentStores {}

impl LineComment {
    pub fn last_line(&self) -> u32 {
        self.end_line.unwrap_or(self.line).max(self.line)
    }
}

impl CommentStore {
    /// The store of a project root. Editors of one root share one store and one watch.
    pub fn for_root(root: &Path, fs: Arc<dyn Fs>, cx: &mut App) -> Entity<CommentStore> {
        if let Some(store) = cx
            .default_global::<CommentStores>()
            .0
            .get(root)
            .and_then(WeakEntity::upgrade)
        {
            return store;
        }
        let store = cx.new(|cx| CommentStore {
            files: BTreeMap::new(),
            _watch: cx.spawn({
                let root = root.to_path_buf();
                async move |store, cx| watch_store(store, root, fs, cx).await
            }),
        });
        cx.default_global::<CommentStores>()
            .0
            .insert(root.to_path_buf(), store.downgrade());
        store
    }

    /// The comments of one file, by its path from the root with `/` separators.
    pub fn comments(&self, path: &str) -> &[LineComment] {
        self.files.get(path).map(Vec::as_slice).unwrap_or_default()
    }
}

fn store_path(root: &Path) -> PathBuf {
    root.join(".tmp").join("line-comment.json")
}

async fn watch_store(
    store: WeakEntity<CommentStore>,
    root: PathBuf,
    fs: Arc<dyn Fs>,
    cx: &mut gpui::AsyncApp,
) {
    let path = store_path(&root);
    let tmp_dir = root.join(".tmp");
    let (mut events, watcher) = fs.watch(&root, STORE_WATCH_LATENCY).await;
    // Linux watches are not recursive, so the folder of the store needs its own watch.
    watcher.add(&tmp_dir).ok();
    loop {
        let files = load_store(fs.as_ref(), &path).await.log_err().unwrap_or_default();
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
            if events.iter().any(|event| event.path == tmp_dir) {
                watcher.add(&tmp_dir).ok();
            }
            if events.iter().any(|event| event.path == path) {
                break;
            }
        }
    }
}

async fn load_store(fs: &dyn Fs, path: &Path) -> anyhow::Result<BTreeMap<String, Vec<LineComment>>> {
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
