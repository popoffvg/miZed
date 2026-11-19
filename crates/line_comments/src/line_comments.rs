mod comment_store;
#[cfg(test)]
mod line_comments_tests;

use std::{path::Path, sync::Arc};

use collections::{HashMap, HashSet};
use editor::{
    Editor,
    display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
};
use gpui::{AnyElement, App, Context, Entity, Subscription};
use language::Point;
use ui::prelude::*;

pub use comment_store::{Author, CommentStore, LineComment};

pub fn init(cx: &mut App) {
    cx.observe_new(|editor: &mut Editor, _, cx| register_editor(editor, cx))
        .detach();
}

/// Shows the line comments of every buffer in the editor as a block under the last line
/// each comment covers.
struct LineCommentAddon {
    stores: HashMap<Arc<Path>, (Entity<CommentStore>, Subscription)>,
    block_ids: Vec<CustomBlockId>,
    _buffer_subscription: Subscription,
}

impl editor::Addon for LineCommentAddon {
    fn to_any(&self) -> &dyn std::any::Any {
        self
    }

    fn to_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

fn register_editor(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(project) = editor.project() else {
        return;
    };
    if !editor.mode().is_full() || !project.read(cx).is_local() {
        return;
    }
    let buffer_subscription =
        cx.subscribe(&editor.buffer().clone(), |editor, _, event, cx| match event {
            multi_buffer::Event::BufferRangesUpdated { .. }
            | multi_buffer::Event::BuffersRemoved { .. }
            | multi_buffer::Event::FileHandleChanged => show_comments(editor, cx),
            _ => {}
        });
    editor.register_addon(LineCommentAddon {
        stores: HashMap::default(),
        block_ids: Vec::new(),
        _buffer_subscription: buffer_subscription,
    });
    show_comments(editor, cx);
}

fn show_comments(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(fs) = editor.project().map(|project| project.read(cx).fs().clone()) else {
        return;
    };
    let buffers = editor
        .buffer()
        .read(cx)
        .all_buffers()
        .into_iter()
        .filter_map(|buffer| {
            let file = project::File::from_dyn(buffer.read(cx).file())?;
            let root: Arc<Path> = file.worktree.read(cx).abs_path();
            let path = file.path.as_unix_str().to_string();
            Some((buffer, root, path))
        })
        .collect::<Vec<_>>();

    let mut stores = editor
        .addon_mut::<LineCommentAddon>()
        .map(|addon| std::mem::take(&mut addon.stores))
        .unwrap_or_default();
    for (_, root, _) in &buffers {
        if !stores.contains_key(root) {
            let store = CommentStore::for_root(root, fs.clone(), cx);
            let subscription = cx.observe(&store, |editor, _, cx| show_comments(editor, cx));
            stores.insert(root.clone(), (store, subscription));
        }
    }

    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let mut blocks = Vec::new();
    let mut labels = Vec::new();
    for (buffer, root, path) in &buffers {
        let buffer = buffer.read(cx).snapshot();
        let max_row = buffer.max_point().row;
        for comment in stores[root].0.read(cx).comments(path) {
            let row = comment.last_line().saturating_sub(1).min(max_row);
            let anchor = buffer.anchor_before(Point::new(row, buffer.line_len(row)));
            let Some(anchor) = snapshot.anchor_in_excerpt(anchor) else {
                continue;
            };
            let label = comment_label(comment);
            labels.push(label.clone());
            blocks.push(BlockProperties {
                placement: BlockPlacement::Below(anchor),
                height: Some(1),
                style: BlockStyle::Sticky,
                render: Arc::new(move |cx| render_comment(label.clone(), cx)),
                priority: 0,
            });
        }
    }

    let Some(addon) = editor.addon_mut::<LineCommentAddon>() else {
        return;
    };
    addon.stores = stores;
    let old_block_ids = std::mem::take(&mut addon.block_ids);
    editor.remove_blocks(old_block_ids.into_iter().collect::<HashSet<_>>(), None, cx);
    let block_ids = editor.insert_blocks(blocks, None, cx);
    #[cfg(test)]
    for (block_id, label) in block_ids.iter().zip(labels) {
        editor::test::set_block_content_for_tests(&cx.entity(), *block_id, cx, move |_| {
            label.to_string()
        });
    }
    if let Some(addon) = editor.addon_mut::<LineCommentAddon>() {
        addon.block_ids = block_ids;
    }
}

fn comment_label(comment: &LineComment) -> SharedString {
    let author = match comment.author {
        Author::Human => "",
        Author::Agent => "Claude: ",
    };
    let orphaned = if comment.orphaned { " (orphaned)" } else { "" };
    format!("{author}{}{orphaned}", comment.text.replace('\n', " ")).into()
}

fn render_comment(label: SharedString, cx: &mut BlockContext) -> AnyElement {
    h_flex()
        .pl(cx.margins.gutter.full_width())
        .gap_1()
        .child(
            Icon::new(IconName::Chat)
                .size(IconSize::Small)
                .color(Color::Muted),
        )
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate(),
        )
        .into_any_element()
}
