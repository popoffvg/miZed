mod comment_store;
#[cfg(test)]
mod line_comments_tests;

use std::{ops::Range, path::PathBuf, sync::Arc};

use collections::{HashMap, HashSet};
use editor::{
    Anchor, Editor, ToPoint as _,
    display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
};
use gpui::{
    AnyElement, App, Context, Entity, EntityId, Global, Subscription, Task, WeakEntity, Window,
    actions,
};
use language::{LocalFile as _, Point};
use ui::prelude::*;
use workspace::Workspace;

pub use comment_store::{Author, CommentStore, LineComment};

actions!(
    git,
    [
        /// Opens a prompt under the selected lines to write or change their line comment.
        /// Empty text deletes the comment.
        EditLineComment,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        let project_id = workspace.project().entity_id();
        let workspace = cx.entity().downgrade();
        cx.default_global::<ProjectWorkspaces>()
            .0
            .insert(project_id, workspace);
    })
    .detach();
    cx.observe_new(|editor: &mut Editor, _, cx| register_editor(editor, cx))
        .detach();
}

/// The workspace of each project. An editor knows its project when it is created, and its
/// workspace only later.
#[derive(Default)]
struct ProjectWorkspaces(HashMap<EntityId, WeakEntity<Workspace>>);

impl Global for ProjectWorkspaces {}

/// The lines a comment covers, and the revision of the file the lines were read on.
pub struct CommentTarget {
    pub abs_path: PathBuf,
    pub first_line: u32,
    pub last_line: u32,
    pub revision: Option<String>,
}

/// The 1-based first and last line that a selection covers. A selection that ends at the
/// start of a line does not cover that line.
pub fn selected_lines(range: Range<Point>) -> (u32, u32) {
    let first = range.start.row + 1;
    let last = if range.end.column == 0 && range.end.row > range.start.row {
        range.end.row
    } else {
        range.end.row + 1
    };
    (first, last)
}

/// Shows the line comments of the workspace as a block under the last line each comment
/// covers, in every buffer of the editor.
struct LineCommentAddon {
    store: Option<(Entity<CommentStore>, Subscription)>,
    block_ids: Vec<CustomBlockId>,
    _subscriptions: [Subscription; 2],
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
    let handle = cx.entity().downgrade();
    let action_subscription = editor.register_action(move |_: &EditLineComment, window, cx| {
        handle
            .update(cx, |editor, cx| edit_selected_lines(editor, window, cx))
            .ok();
    });
    editor.register_addon(LineCommentAddon {
        store: None,
        block_ids: Vec::new(),
        _subscriptions: [buffer_subscription, action_subscription],
    });
    show_comments(editor, cx);
}

/// An editor is often created inside an update of its workspace, so the store connects
/// in a later effect, when the workspace can be read.
fn connect_store(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(project) = editor.project().cloned() else {
        return;
    };
    let Some(workspace_id) = cx
        .try_global::<ProjectWorkspaces>()
        .and_then(|workspaces| workspaces.0.get(&project.entity_id()))
        .and_then(WeakEntity::upgrade)
        .and_then(|workspace| workspace.read(cx).database_id())
    else {
        return;
    };
    let fs = project.read(cx).fs().clone();
    let store = CommentStore::for_workspace(workspace_id, fs, cx);
    let subscription = cx.observe(&store, |editor, _, cx| show_comments(editor, cx));
    let Some(addon) = editor.addon_mut::<LineCommentAddon>() else {
        return;
    };
    addon.store = Some((store, subscription));
    show_comments(editor, cx);
}

fn connected_store(editor: &Editor) -> Option<Entity<CommentStore>> {
    let (store, _) = editor.addon::<LineCommentAddon>()?.store.as_ref()?;
    Some(store.clone())
}

fn show_comments(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(store) = connected_store(editor) else {
        let handle = cx.entity().downgrade();
        cx.defer(move |cx| {
            handle.update(cx, connect_store).ok();
        });
        return;
    };
    let store = store.read(cx);
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let mut blocks = Vec::new();
    let mut labels = Vec::new();
    for buffer in editor.buffer().read(cx).all_buffers() {
        let buffer = buffer.read(cx);
        let Some(file) = project::File::from_dyn(buffer.file()) else {
            continue;
        };
        let path = file.abs_path(cx).to_string_lossy().into_owned();
        let buffer = buffer.snapshot();
        let max_row = buffer.max_point().row;
        for comment in store.comments(&path) {
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

/// Handles the action for a buffer of a project file. Other buffers, such as the files of
/// a past commit, go on to the handler of the view that shows them.
fn edit_selected_lines(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let selection = *editor.selections.newest_anchor();
    let range = selection.start.to_point(&snapshot)..selection.end.to_point(&snapshot);
    let Some((buffer, range)) = snapshot.range_to_buffer_range(range) else {
        cx.propagate();
        return;
    };
    let Some(file) = project::File::from_dyn(buffer.file()) else {
        cx.propagate();
        return;
    };
    let (first_line, last_line) = selected_lines(range);
    let target = CommentTarget {
        abs_path: file.abs_path(cx),
        first_line,
        last_line,
        revision: None,
    };
    let task = edit_comment(editor, selection.start, target, window, cx);
    editor.detach_and_notify_err(task, window, cx);
}

/// Opens the prompt at `anchor` with the comment that starts on the first target line,
/// and saves what the user confirms.
pub fn edit_comment(
    editor: &mut Editor,
    anchor: Anchor,
    target: CommentTarget,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Task<anyhow::Result<()>> {
    let Some(store) = connected_store(editor) else {
        return Task::ready(Err(anyhow::anyhow!(
            "Line comments need a saved workspace"
        )));
    };
    let path = target.abs_path.to_string_lossy().into_owned();
    let value = store
        .read(cx)
        .comments(&path)
        .iter()
        .find(|comment| comment.line == target.first_line)
        .map(|comment| comment.text.clone())
        .unwrap_or_default();
    let (sender, receiver) = async_channel::bounded(1);
    editor.show_inline_text_prompt(anchor, "Comment…", &value, sender, window, cx);
    cx.spawn(async move |_, cx| {
        let Ok(Some(text)) = receiver.recv().await else {
            return Ok(());
        };
        let text = with_revision(text.trim(), target.revision.as_deref());
        store
            .update(cx, |store, cx| {
                if text.is_empty() {
                    store.remove(&path, target.first_line, cx)
                } else {
                    let comment = LineComment {
                        line: target.first_line,
                        end_line: (target.last_line > target.first_line)
                            .then_some(target.last_line),
                        text,
                        orphaned: false,
                        author: Author::Human,
                    };
                    store.upsert(&path, comment, cx)
                }
            })
            .await
    })
}

/// A comment written on a past commit names that commit at the end, because the store
/// keeps the line as a line of the working tree.
fn with_revision(text: &str, revision: Option<&str>) -> String {
    let Some(revision) = revision else {
        return text.to_string();
    };
    let read_on = format!("read on {revision}");
    if text.is_empty() || text.ends_with(&read_on) {
        text.to_string()
    } else {
        format!("{text}\n\n{read_on}")
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
