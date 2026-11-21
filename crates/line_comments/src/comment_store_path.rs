use std::{fmt, path::PathBuf, str::FromStr};

const DEFAULT_COMMENT_STORE: &str = "default";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CommentStoreName(String);

impl Default for CommentStoreName {
    fn default() -> Self {
        Self(DEFAULT_COMMENT_STORE.into())
    }
}

impl fmt::Display for CommentStoreName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for CommentStoreName {
    type Err = anyhow::Error;

    fn from_str(name: &str) -> anyhow::Result<Self> {
        let name = name.trim();
        anyhow::ensure!(
            (1..=64).contains(&name.len())
                && name.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                }),
            "Use 1 to 64 lowercase letters, digits, or hyphens for a comment store name"
        );
        Ok(Self(name.into()))
    }
}

pub(crate) fn line_comment_store_path(workspace_id: i64, name: &CommentStoreName) -> PathBuf {
    let folder = paths::data_dir().join("line_comments");
    if name.0 == DEFAULT_COMMENT_STORE {
        folder.join(format!("{workspace_id}.json"))
    } else {
        folder
            .join(workspace_id.to_string())
            .join(format!("store-{name}.json"))
    }
}
