mod comment_store;
mod comment_store_path;
mod line_comment_panel;
mod line_comment_picker;
#[cfg(test)]
mod line_comments_tests;

use std::{ops::Range, path::PathBuf, sync::Arc};

use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use editor::{
    Anchor, Editor, RowHighlightOptions, SplittableEditor, ToPoint as _,
    display_map::{BlockContext, BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
};
use git_ui::{commit_view::CommitView, solo_diff_view::SoloDiffView};
use gpui::{
    AnyElement, App, Context, Entity, EntityId, Global, Subscription, Task, WeakEntity, Window,
    actions,
};
use language::{DiskState, LocalFile as _, Point};
use ui::{ContextMenu, Tooltip, prelude::*, right_click_menu};
use workspace::{
    Workspace,
    notifications::{NotifyResultExt as _, NotifyTaskExt as _},
};

pub use comment_store::{Author, CommentStore, LineComment};
pub use comment_store_path::CommentStoreName;
pub use line_comment_panel::add_line_comments_panel;

actions!(
    line_comments,
    [
        /// Adds a comment to the selected lines.
        AddLineComment,
        /// Opens the comment on the current line for editing. Empty text deletes it.
        #[action(deprecated_aliases = ["git::EditLineComment"])]
        EditLineComment,
        /// Searches comments in the active workspace store.
        ListLineComments,
        /// Selects the workspace store used to show and edit line comments.
        SelectLineCommentStore,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        workspace.register_action(|workspace, _: &AddLineComment, window, cx| {
            prompt_diff_comment(workspace, CommentAction::Add, window, cx)
        });
        workspace.register_action(|workspace, _: &EditLineComment, window, cx| {
            prompt_diff_comment(workspace, CommentAction::Edit, window, cx)
        });
        workspace.register_action(line_comment_picker::list_line_comments);
        workspace.register_action(line_comment_panel::toggle_line_comments_panel);
        let project_id = workspace.project().entity_id();
        let workspace_id = cx.entity_id();
        let workspace = cx.weak_entity();
        let selection = cx.new(|_| CommentStoreSelection::default());
        cx.default_global::<ProjectWorkspaces>().0.insert(
            project_id,
            WorkspaceCommentStores {
                workspace,
                selection,
            },
        );
        cx.on_release(move |_, cx| {
            let projects = &mut cx.default_global::<ProjectWorkspaces>().0;
            if projects
                .get(&project_id)
                .is_some_and(|stores| stores.workspace.entity_id() == workspace_id)
            {
                projects.remove(&project_id);
            }
        })
        .detach();
    })
    .detach();
    cx.observe_new(|editor: &mut Editor, _, cx| register_editor(editor, cx))
        .detach();
}

/// The workspace of each project. An editor knows its project when it is created, and its
/// workspace only later.
#[derive(Default)]
struct ProjectWorkspaces(HashMap<EntityId, WorkspaceCommentStores>);

impl Global for ProjectWorkspaces {}

struct WorkspaceCommentStores {
    workspace: WeakEntity<Workspace>,
    selection: Entity<CommentStoreSelection>,
}

#[derive(Default)]
struct CommentStoreSelection(CommentStoreName);

/// The lines a comment covers, and the revision of the file the lines were read on.
#[derive(Clone)]
struct CommentTarget {
    abs_path: PathBuf,
    first_line: u32,
    last_line: u32,
    revision: Option<String>,
}

#[derive(Clone, Copy)]
enum CommentAction {
    Add,
    Edit,
}

/// The 1-based first and last line that a selection covers. A selection that ends at the
/// start of a line does not cover that line.
fn selected_lines(range: Range<Point>) -> (u32, u32) {
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
    editor: WeakEntity<Editor>,
    store: Option<(Entity<CommentStore>, Subscription)>,
    selection_subscription: Option<Subscription>,
    block_ids: Vec<CustomBlockId>,
    collapsed_comments: HashSet<(PathBuf, u32)>,
    _subscriptions: [Subscription; 4],
}

enum LineCommentTargetHighlights {}

impl editor::Addon for LineCommentAddon {
    fn extend_gutter_context_menu(
        &self,
        mut menu: ContextMenu,
        editor: &Editor,
        anchor: Anchor,
        cx: &App,
    ) -> ContextMenu {
        let Some((store, _)) = &self.store else {
            return menu;
        };
        let store = store.read(cx);
        let Some(line_target) = editor_comment_target(editor, anchor..anchor, cx) else {
            return menu;
        };
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let selection = editor.selections.newest_anchor();
        let first = selection.start.to_point(&snapshot);
        let last = selection.end.to_point(&snapshot);
        let row = anchor.to_point(&snapshot).row + 1;
        let (first_line, last_line) = selected_lines(first..last);
        let selection_target = (first_line..=last_line)
            .contains(&row)
            .then(|| editor_comment_target(editor, selection.start..selection.end, cx))
            .flatten();
        let add_target = selection_target.unwrap_or_else(|| line_target.clone());
        let can_add = !store
            .comments(&add_target.abs_path.to_string_lossy())
            .iter()
            .any(|comment| comment.line == add_target.first_line);
        let existing_comment = line_comment_at(
            store.comments(&line_target.abs_path.to_string_lossy()),
            line_target.first_line,
        );
        if can_add || existing_comment.is_some() {
            menu = menu.separator();
        }
        if can_add {
            let editor = self.editor.clone();
            menu = menu.entry(
                "Add Line Comment",
                Some(Box::new(AddLineComment)),
                move |window, cx| {
                    editor
                        .update(cx, |editor, cx| {
                            let task = prompt_line_comment(
                                editor,
                                anchor,
                                add_target.clone(),
                                CommentAction::Add,
                                window,
                                cx,
                            );
                            editor.detach_and_notify_err(task, window, cx);
                        })
                        .ok();
                },
            );
        }
        if let Some(comment) = existing_comment {
            let done = comment.done;
            let edit_editor = self.editor.clone();
            let done_editor = self.editor.clone();
            let mut target = line_target;
            target.first_line = comment.line;
            target.last_line = comment.last_line();
            menu = menu
                .entry("Edit Line Comment", Some(Box::new(EditLineComment)), {
                    let target = target.clone();
                    move |window, cx| {
                        edit_editor
                            .update(cx, |editor, cx| {
                                let task = prompt_line_comment(
                                    editor,
                                    anchor,
                                    target.clone(),
                                    CommentAction::Edit,
                                    window,
                                    cx,
                                );
                                editor.detach_and_notify_err(task, window, cx);
                            })
                            .ok();
                    }
                })
                .entry(comment_done_icon(done).1, None, move |window, cx| {
                    update_comment_done(&done_editor, &target, !done, window, cx);
                });
        }
        menu
    }

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
        cx.subscribe(
            &editor.buffer().clone(),
            |editor, _, event, cx| match event {
                multi_buffer::Event::BufferRangesUpdated { .. }
                | multi_buffer::Event::BuffersRemoved { .. }
                | multi_buffer::Event::FileHandleChanged
                | multi_buffer::Event::Edited { .. } => show_comments(editor, cx),
                _ => {}
            },
        );
    let handle = cx.entity().downgrade();
    let add_subscription = editor.register_action(move |_: &AddLineComment, window, cx| {
        handle
            .update(cx, |editor, cx| {
                prompt_selected_lines(editor, CommentAction::Add, window, cx)
            })
            .ok();
    });
    let handle = cx.weak_entity();
    let edit_subscription = editor.register_action(move |_: &EditLineComment, window, cx| {
        handle
            .update(cx, |editor, cx| {
                prompt_selected_lines(editor, CommentAction::Edit, window, cx)
            })
            .ok();
    });
    let handle = cx.weak_entity();
    let select_subscription =
        editor.register_action(move |_: &SelectLineCommentStore, window, cx| {
            handle
                .update(cx, |editor, cx| select_store(editor, window, cx))
                .ok();
        });
    editor.register_addon(LineCommentAddon {
        editor: cx.weak_entity(),
        store: None,
        selection_subscription: None,
        block_ids: Vec::new(),
        collapsed_comments: HashSet::default(),
        _subscriptions: [
            buffer_subscription,
            add_subscription,
            edit_subscription,
            select_subscription,
        ],
    });
    show_comments(editor, cx);
}

/// An editor is often created inside an update of its workspace, so the store connects
/// in a later effect, when the workspace can be read.
fn connect_store(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(project) = editor.project().cloned() else {
        return;
    };
    let Some(stores) = cx
        .try_global::<ProjectWorkspaces>()
        .and_then(|workspaces| workspaces.0.get(&project.entity_id()))
    else {
        return;
    };
    let Some(workspace_id) = stores
        .workspace
        .upgrade()
        .and_then(|workspace| workspace.read(cx).database_id())
    else {
        return;
    };
    let selection = stores.selection.clone();
    let store = workspace_comment_store(workspace_id, &selection, &project, cx);
    let subscription = cx.observe(&store, |editor, _, cx| show_comments(editor, cx));
    let Some(addon) = editor.addon_mut::<LineCommentAddon>() else {
        return;
    };
    if addon
        .store
        .as_ref()
        .is_some_and(|(current, _)| *current != store)
    {
        addon.collapsed_comments.clear();
    }
    addon.store = Some((store, subscription));
    if addon.selection_subscription.is_none() {
        addon.selection_subscription = Some(cx.observe(&selection, |editor, _, cx| {
            connect_store(editor, cx);
        }));
    }
    show_comments(editor, cx);
}

fn workspace_comment_store(
    workspace_id: workspace::WorkspaceId,
    selection: &Entity<CommentStoreSelection>,
    project: &Entity<project::Project>,
    cx: &mut App,
) -> Entity<CommentStore> {
    let name = selection.read(cx).0.clone();
    let fs = project.read(cx).fs().clone();
    CommentStore::workspace_comment_store(workspace_id, name, fs, cx)
}

fn select_store(editor: &mut Editor, window: &mut Window, cx: &mut Context<Editor>) {
    let Some(selection) = editor.project().and_then(|project| {
        cx.try_global::<ProjectWorkspaces>()?
            .0
            .get(&project.entity_id())
            .map(|stores| stores.selection.clone())
    }) else {
        return;
    };
    let anchor = editor.selections.newest_anchor().start;
    let name = selection.read(cx).0.to_string();
    let (sender, receiver) = async_channel::bounded(1);
    editor.show_inline_text_prompt(anchor, "Line comment store", &name, sender, window, cx);
    let task = cx.spawn(async move |_, cx| {
        let Ok(Some(name)) = receiver.recv().await else {
            return Ok(());
        };
        let name = name.parse()?;
        selection.update(cx, |selection, cx| {
            if selection.0 != name {
                selection.0 = name;
                cx.notify();
            }
        });
        anyhow::Ok(())
    });
    editor.detach_and_notify_err(task, window, cx);
}

fn editor_comment_store(editor: &Editor) -> Option<Entity<CommentStore>> {
    let (store, _) = editor.addon::<LineCommentAddon>()?.store.as_ref()?;
    Some(store.clone())
}

fn show_comments(editor: &mut Editor, cx: &mut Context<Editor>) {
    let Some(store) = editor_comment_store(editor) else {
        let handle = cx.entity().downgrade();
        cx.defer(move |cx| {
            handle.update(cx, connect_store).ok();
        });
        return;
    };
    let Some(addon) = editor.addon::<LineCommentAddon>() else {
        return;
    };
    let store = store.read(cx);
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let mut blocks = Vec::new();
    let mut target_ranges = Vec::new();
    #[cfg(test)]
    let mut labels = Vec::new();
    for buffer in editor.buffer().read(cx).all_buffers() {
        let buffer = buffer.read(cx);
        let abs_path = buffer
            .file()
            .and_then(|file| file.as_local())
            .map(|file| file.abs_path(cx))
            .or_else(|| {
                let workspace = editor.workspace()?;
                workspace
                    .read(cx)
                    .items_of_type::<CommitView>(cx)
                    .find_map(|view| view.read(cx).working_tree_path(buffer, cx))
            });
        let Some(abs_path) = abs_path else {
            continue;
        };
        let path = abs_path.to_string_lossy();
        let buffer = buffer.snapshot();
        let max_row = buffer.max_point().row;
        let excerpts = snapshot
            .excerpts_for_buffer(buffer.remote_id())
            .collect::<Vec<_>>();
        for comment in store.comments(&path) {
            let row = comment.last_line().saturating_sub(1).min(max_row);
            let first_row = comment.line.saturating_sub(1).min(row);
            let start = buffer.anchor_before(Point::new(first_row, 0));
            let end = buffer.anchor_before(Point::new(row, buffer.line_len(row)));
            let comment_key = (abs_path.clone(), comment.line);
            let done = comment.done;
            let label = (!addon.collapsed_comments.contains(&comment_key))
                .then(|| multiline_comment_label(comment));
            let height = label.as_ref().map_or(1, |label| {
                u32::try_from(label.split('\n').count()).unwrap_or(u32::MAX)
            });
            let editor = cx.weak_entity();
            let target = CommentTarget {
                abs_path: comment_key.0,
                first_line: comment.line,
                last_line: comment.last_line(),
                revision: None,
            };
            for excerpt in &excerpts {
                let excerpt = &excerpt.context;
                let start =
                    std::cmp::max_by(start, excerpt.start, |left, right| left.cmp(right, &buffer));
                let end =
                    std::cmp::min_by(end, excerpt.end, |left, right| left.cmp(right, &buffer));
                if start.cmp(&end, &buffer).is_gt() {
                    continue;
                }
                let Some(visible) = snapshot.anchor_range_in_buffer(start..end) else {
                    continue;
                };
                let anchor = visible.end;
                let end = if buffer.line_len(row) == 0 && row < max_row {
                    let next_line = buffer.anchor_before(Point::new(row + 1, 0));
                    if next_line.cmp(&excerpt.end, &buffer).is_le() {
                        snapshot.anchor_in_buffer(next_line).unwrap_or(anchor)
                    } else {
                        anchor
                    }
                } else if buffer.line_len(row) == 0 && snapshot.as_singleton().is_some() {
                    Anchor::Max
                } else {
                    anchor
                };
                target_ranges.push(visible.start..end);
                #[cfg(test)]
                labels.push(label.clone());
                let editor = editor.clone();
                let target = target.clone();
                let label = label.clone();
                blocks.push(BlockProperties {
                    placement: BlockPlacement::Below(anchor),
                    height: Some(height),
                    style: BlockStyle::Sticky,
                    render: Arc::new(move |cx| {
                        render_comment(label.clone(), done, &editor, anchor, &target, cx)
                    }),
                    priority: 0,
                });
            }
        }
    }

    let Some(addon) = editor.addon_mut::<LineCommentAddon>() else {
        return;
    };
    let old_block_ids = std::mem::take(&mut addon.block_ids);
    editor.clear_row_highlights::<LineCommentTargetHighlights>();
    editor.remove_blocks(old_block_ids.into_iter().collect::<HashSet<_>>(), None, cx);
    let block_ids = editor.insert_blocks(blocks, None, cx);
    for range in target_ranges {
        editor.highlight_rows::<LineCommentTargetHighlights>(
            range,
            |cx| Color::Info.color(cx).alpha(0.22),
            RowHighlightOptions::default(),
            cx,
        );
    }
    #[cfg(test)]
    for (block_id, label) in block_ids.iter().zip(labels) {
        editor::test::set_block_content_for_tests(&cx.entity(), *block_id, cx, move |_| {
            label
                .as_ref()
                .map_or_else(|| " ".to_string(), |label| format!("{label} "))
        });
    }
    if let Some(addon) = editor.addon_mut::<LineCommentAddon>() {
        addon.block_ids = block_ids;
    }
}

fn prompt_diff_comment(
    workspace: &mut Workspace,
    action: CommentAction,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(split) = workspace
        .active_item_as::<SplittableEditor>(cx)
        .or_else(|| {
            workspace
                .active_item_as::<SoloDiffView>(cx)
                .map(|view| view.read(cx).diff_editor().clone())
        })
    else {
        cx.propagate();
        return;
    };
    let result = diff_comment_target(workspace, &split, cx);
    let Some((editor, anchor, target)) = result.notify_err(workspace, cx) else {
        return;
    };
    let task = editor.update(cx, |editor, cx| {
        prompt_line_comment(editor, anchor, target, action, window, cx)
    });
    task.detach_and_notify_err(cx.weak_entity(), window, cx);
}

fn diff_comment_target(
    workspace: &Workspace,
    split: &Entity<SplittableEditor>,
    cx: &mut App,
) -> Result<(Entity<Editor>, Anchor, CommentTarget)> {
    anyhow::ensure!(
        workspace.project().read(cx).is_local(),
        "Diff comments require a local project"
    );
    let split = split.read(cx);
    let editor = split.focused_editor().clone();
    let rhs = split.rhs_editor().clone();
    let view = editor.read(cx);
    let buffer = view.buffer().read(cx);
    let snapshot = buffer.snapshot(cx);
    let selection = view.selections.newest_anchor();
    let anchor = selection.start;
    let (source, range) = snapshot
        .range_to_buffer_range(
            selection.start.to_point(&snapshot)..selection.end.to_point(&snapshot),
        )
        .context("Select lines from one side of one file")?;
    let main_id = anchor.buffer_id().unwrap_or(source.remote_id());
    let rhs_id = buffer
        .diff_for(main_id)
        .map_or(main_id, |diff| diff.read(cx).buffer_id);
    let document = rhs
        .read(cx)
        .buffer()
        .read(cx)
        .buffer(rhs_id)
        .context("Cannot find the diff file")?;
    let document = document.read(cx);
    let file = document.file().context("The diff has no file path")?;
    let (path, revision) = if let Some(commit) = workspace.active_item_as::<CommitView>(cx) {
        let commit = commit.read(cx);
        let path = commit
            .working_tree_path(document, cx)
            .context("The commit diff has no working tree path")?;
        let revision = if source.remote_id() == rhs_id
            && !matches!(file.disk_state(), DiskState::Historic { was_deleted: true })
        {
            commit.commit_sha().to_string()
        } else {
            format!("{}^", commit.commit_sha())
        };
        (path, Some(revision))
    } else {
        (
            source
                .file()
                .and_then(|source_file| source_file.as_local())
                .or_else(|| file.as_local())
                .context("The diff has no local file path")?
                .abs_path(cx),
            None,
        )
    };
    let (first_line, last_line) = selected_lines(range);
    let target = CommentTarget {
        abs_path: path,
        first_line,
        last_line,
        revision,
    };
    Ok((editor, anchor, target))
}

/// Handles the action for a buffer of a project file. Other buffers, such as the files of
/// a past commit, go on to the handler of the view that shows them.
fn prompt_selected_lines(
    editor: &mut Editor,
    action: CommentAction,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let selection = *editor.selections.newest_anchor();
    let Some(target) = editor_comment_target(editor, selection.start..selection.end, cx) else {
        cx.propagate();
        return;
    };
    let task = prompt_line_comment(editor, selection.start, target, action, window, cx);
    editor.detach_and_notify_err(task, window, cx);
}

fn editor_comment_target(editor: &Editor, range: Range<Anchor>, cx: &App) -> Option<CommentTarget> {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let range = range.start.to_point(&snapshot)..range.end.to_point(&snapshot);
    let (buffer, range) = snapshot.range_to_buffer_range(range)?;
    let file = project::File::from_dyn(buffer.file())?;
    let (first_line, last_line) = selected_lines(range);
    Some(CommentTarget {
        abs_path: file.abs_path(cx),
        first_line,
        last_line,
        revision: None,
    })
}

fn line_comment_at(comments: &[LineComment], line: u32) -> Option<&LineComment> {
    comments
        .iter()
        .rev()
        .find(|comment| comment.line <= line && line <= comment.last_line())
}

fn prompt_line_comment(
    editor: &mut Editor,
    anchor: Anchor,
    target: CommentTarget,
    action: CommentAction,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> Task<anyhow::Result<()>> {
    let Some(store) = editor_comment_store(editor) else {
        return Task::ready(Err(anyhow::anyhow!("Line comments need a saved workspace")));
    };
    if !store.read(cx).is_loaded() {
        return Task::ready(Err(anyhow::anyhow!("The line comment store is not ready")));
    }
    let path = target.abs_path.to_string_lossy().into_owned();
    let comments = store.read(cx).comments(&path);
    let existing = comments
        .iter()
        .find(|comment| comment.line == target.first_line);
    let mut comment = match action {
        CommentAction::Add if existing.is_some() => {
            return Task::ready(Err(anyhow::anyhow!(
                "A comment already starts on this line"
            )));
        }
        CommentAction::Add => LineComment {
            line: target.first_line,
            end_line: (target.last_line > target.first_line).then_some(target.last_line),
            text: String::new(),
            done: false,
            orphaned: false,
            author: Author::Human,
        },
        CommentAction::Edit => {
            let Some(comment) = existing.or_else(|| line_comment_at(comments, target.first_line))
            else {
                return Task::ready(Err(anyhow::anyhow!("There is no comment on this line")));
            };
            comment.clone()
        }
    };
    let (sender, receiver) = async_channel::bounded(1);
    editor.show_inline_multiline_prompt(anchor, "Comment…", &comment.text, sender, window, cx);
    let mut original = comment.clone();
    cx.spawn(async move |_, cx| {
        let Ok(Some(text)) = receiver.recv().await else {
            return Ok(());
        };
        comment.text = with_revision(text.trim(), target.revision.as_deref());
        if comment.text.is_empty() && matches!(action, CommentAction::Add) {
            return Ok(());
        }
        store
            .update(cx, |store, cx| {
                if matches!(action, CommentAction::Add) {
                    return store.add_comment(&path, comment, cx);
                }
                let Some(current) = store
                    .comments(&path)
                    .iter()
                    .find(|current| current.line == comment.line)
                else {
                    return Task::ready(Err(anyhow::anyhow!(
                        "The edited line comment no longer exists"
                    )));
                };
                original.done = current.done;
                if *current != original {
                    return Task::ready(Err(anyhow::anyhow!(
                        "The line comment changed while the editor was open"
                    )));
                }
                comment.done = current.done;
                if comment.text.is_empty() {
                    store.remove(&path, comment.line, cx)
                } else {
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
    multiline_comment_label(comment).replace('\n', " ").into()
}

fn multiline_comment_label(comment: &LineComment) -> SharedString {
    let author = match comment.author {
        Author::Human => "",
        Author::Agent => "Claude: ",
    };
    let orphaned = if comment.orphaned { " (orphaned)" } else { "" };
    format!("{author}{}{orphaned}", comment.text).into()
}

fn comment_done_icon(done: bool) -> (IconName, &'static str) {
    if done {
        (IconName::ChatFilled, "Reopen Comment")
    } else {
        (IconName::Chat, "Mark Comment Done")
    }
}

fn update_comment_done(
    editor: &WeakEntity<Editor>,
    target: &CommentTarget,
    done: bool,
    window: &mut Window,
    cx: &mut App,
) {
    editor
        .update(cx, |editor, cx| {
            if let Some(store) = editor_comment_store(editor) {
                let task = store.update(cx, |store, cx| {
                    store.set_comment_done(
                        &target.abs_path.to_string_lossy(),
                        target.first_line,
                        done,
                        cx,
                    )
                });
                editor.detach_and_notify_err(task, window, cx);
            }
        })
        .ok();
}

fn render_comment(
    label: Option<SharedString>,
    done: bool,
    editor: &WeakEntity<Editor>,
    anchor: Anchor,
    target: &CommentTarget,
    cx: &mut BlockContext,
) -> AnyElement {
    let toggle_editor = editor.clone();
    let edit_editor = editor.clone();
    let comment_key = (target.abs_path.clone(), target.first_line);
    let target = target.clone();
    let expanded = label.is_some();
    let (icon, tooltip) = comment_done_icon(done);
    let done_target = target.clone();
    let button = IconButton::new("toggle-line-comment", icon)
        .icon_size(IconSize::XSmall)
        .size(ButtonSize::None)
        .icon_color(Color::Warning)
        .aria_label(tooltip)
        .tooltip(Tooltip::text(tooltip))
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            update_comment_done(&toggle_editor, &done_target, !done, window, cx);
        });
    let menu = right_click_menu(cx.block_id)
        .trigger(move |_, _, _| {
            div()
                .debug_selector(|| "toggle-line-comment".to_string())
                .child(button)
        })
        .menu(move |window, cx| {
            let editor = edit_editor.clone();
            let target = target.clone();
            let collapse_editor = editor.clone();
            let comment_key = comment_key.clone();
            ContextMenu::build(window, cx, move |menu, _, _| {
                menu.entry(
                    "Edit Line Comment",
                    Some(Box::new(EditLineComment)),
                    move |window, cx| {
                        editor
                            .update(cx, |editor, cx| {
                                let task = prompt_line_comment(
                                    editor,
                                    anchor,
                                    target.clone(),
                                    CommentAction::Edit,
                                    window,
                                    cx,
                                );
                                editor.detach_and_notify_err(task, window, cx);
                            })
                            .ok();
                    },
                )
                .entry(
                    if expanded {
                        "Collapse Line Comment"
                    } else {
                        "Expand Line Comment"
                    },
                    None,
                    move |_, cx| {
                        collapse_editor
                            .update(cx, |editor, cx| {
                                if let Some(addon) = editor.addon_mut::<LineCommentAddon>() {
                                    if !addon.collapsed_comments.remove(&comment_key) {
                                        addon.collapsed_comments.insert(comment_key.clone());
                                    }
                                    show_comments(editor, cx);
                                }
                            })
                            .ok();
                    },
                )
            })
        });
    let gutter = cx.margins.gutter;
    let line_height = cx.line_height;
    h_flex()
        .w_full()
        .items_start()
        .bg(Color::Warning.color(cx).alpha(0.19))
        .child(
            h_flex()
                .h(line_height)
                .flex_shrink_0()
                .when(gutter.width > gpui::Pixels::ZERO, |gutter_slot| {
                    gutter_slot
                        .w(gutter.full_width())
                        .pr(gutter.right_padding + gpui::px(2.))
                        .justify_end()
                })
                .child(menu),
        )
        .when_some(label, |row, label| {
            row.child(
                v_flex()
                    .font(cx.editor_style.text.font())
                    .text_size(cx.editor_style.text.font_size)
                    .text_color(cx.editor_style.text.color)
                    .line_height(line_height)
                    .flex_1()
                    .min_w_0()
                    .children(label.split('\n').map(move |line| {
                        div()
                            .w_full()
                            .min_h(line_height)
                            .whitespace_normal()
                            .child(SharedString::from(line.trim_end_matches('\r')))
                    })),
            )
        })
        .into_any_element()
}
