mod comment_store_path;

use std::{fs, io};

use anyhow::Context as _;
use clap::Parser;

use comment_store_path::{CommentStoreName, line_comment_store_path};

#[derive(Parser)]
#[command(about = "Print a workspace's line comments as JSON")]
struct LineCommentArgs {
    workspace_id: i64,
    #[arg(long, default_value_t)]
    store: CommentStoreName,
    #[arg(long, value_name = "DIR", value_hint = clap::ValueHint::DirPath)]
    user_data_dir: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let args = LineCommentArgs::parse();
    if let Some(directory) = args.user_data_dir {
        paths::set_custom_data_dir(&directory);
    }

    let path = line_comment_store_path(args.workspace_id, &args.store);
    match fs::read_to_string(&path) {
        Ok(contents) => println!("{contents}"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => println!("null"),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Could not list line comments from {}", path.display()));
        }
    }
    Ok(())
}
