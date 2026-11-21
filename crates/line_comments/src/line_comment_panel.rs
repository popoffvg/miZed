use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use editor::{Editor, EditorEvent};
use file_icons::FileIcons;
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, ListAlignment, ListState,
    Pixels, Render, Subscription, WeakEntity, Window, actions, list, px,
};
use project::Project;
use ui::{
    ContextMenu, ContextMenuEntry, Divider, ListItem, ListItemSpacing, PopoverMenu, Tooltip,
    prelude::*,
};
use workspace::{
    Workspace, WorkspaceId,
    dock::{DockPosition, Panel, PanelEvent},
    notifications::NotifyTaskExt as _,
};

use crate::{
    CommentStore, CommentStoreSelection, ProjectWorkspaces, comment_done_icon,
    line_comment_picker::{LineCommentMatch, match_line_comments, open_comment_location},
    multiline_comment_label, workspace_comment_store,
};

actions!(line_comment_panel, [ToggleFocus]);

pub fn add_line_comments_panel(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    if !project.read(cx).is_local() {
        return;
    }
    let Some(selection) = cx
        .try_global::<ProjectWorkspaces>()
        .and_then(|workspaces| workspaces.0.get(&project.entity_id()))
        .map(|stores| stores.selection.clone())
    else {
        return;
    };
    let workspace_handle = cx.weak_entity();
    let workspace_id = workspace.database_id();
    let panel = cx.new(|cx| {
        LineCommentsPanel::new(
            workspace_handle,
            workspace_id,
            project,
            selection,
            window,
            cx,
        )
    });
    workspace.add_panel(panel, window, cx);
}

pub(crate) fn toggle_line_comments_panel(
    workspace: &mut Workspace,
    _: &ToggleFocus,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    workspace.toggle_panel_focus::<LineCommentsPanel>(window, cx);
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum CommentLayout {
    #[default]
    Grouped,
    Flat,
}

impl CommentLayout {
    const OPTIONS: [(Self, &'static str); 2] =
        [(Self::Grouped, "Grouped"), (Self::Flat, "Flat List")];
}

struct CommentRow {
    depth: usize,
    entry: CommentEntry,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum CommentStatus {
    Open,
    Done,
}

impl CommentStatus {
    const ALL: [Self; 2] = [Self::Open, Self::Done];

    fn label(self) -> &'static str {
        match self {
            Self::Open => "Open",
            Self::Done => "Done",
        }
    }

    fn for_comment(comment: &LineCommentMatch) -> Self {
        if comment.done { Self::Done } else { Self::Open }
    }
}

#[derive(Default)]
struct CollapsedCommentTree {
    sections: HashSet<CommentStatus>,
    paths: HashSet<(CommentStatus, PathBuf)>,
}

enum CommentEntry {
    Status(CommentStatus, usize),
    Folder(CommentStatus, PathBuf),
    File(CommentStatus, PathBuf),
    Comment(LineCommentMatch),
}

fn comment_rows(
    comments: Vec<LineCommentMatch>,
    layout: CommentLayout,
    collapsed: &CollapsedCommentTree,
    project: &Project,
    cx: &App,
) -> Vec<CommentRow> {
    let mut sections = HashMap::<_, Vec<_>>::default();
    for comment in comments {
        sections
            .entry(CommentStatus::for_comment(&comment))
            .or_default()
            .push(comment);
    }
    let mut rows = Vec::new();
    for status in CommentStatus::ALL {
        let Some(mut comments) = sections.remove(&status) else {
            continue;
        };
        rows.push(CommentRow {
            depth: 0,
            entry: CommentEntry::Status(status, comments.len()),
        });
        if collapsed.sections.contains(&status) {
            continue;
        }
        match layout {
            CommentLayout::Flat => {
                comments.sort_by(|left, right| {
                    left.text
                        .cmp(&right.text)
                        .then(left.path.cmp(&right.path))
                        .then(left.line.cmp(&right.line))
                });
                rows.extend(comments.into_iter().map(|comment| CommentRow {
                    depth: 0,
                    entry: CommentEntry::Comment(comment),
                }));
            }
            CommentLayout::Grouped => {
                append_comment_tree(&mut rows, status, comments, &collapsed.paths, project, cx);
            }
        }
    }
    rows
}

fn append_comment_tree(
    rows: &mut Vec<CommentRow>,
    status: CommentStatus,
    mut comments: Vec<LineCommentMatch>,
    collapsed: &HashSet<(CommentStatus, PathBuf)>,
    project: &Project,
    cx: &App,
) {
    comments.sort_by(|left, right| left.path.cmp(&right.path).then(left.line.cmp(&right.line)));
    let mut previous_path: Vec<PathBuf> = Vec::new();
    for comment in comments {
        let root = project
            .find_worktree(&comment.path, cx)
            .map(|(worktree, _)| worktree.read(cx).abs_path());
        let mut path: Vec<_> = comment
            .path
            .ancestors()
            .take_while(|path| root.as_ref().is_some_and(|root| path.starts_with(root)))
            .collect();
        if path.is_empty() {
            path.push(&comment.path);
        }
        path.reverse();

        let shared_depth = previous_path
            .iter()
            .zip(&path)
            .take_while(|(previous, current)| previous.as_path() == **current)
            .count();
        previous_path.truncate(shared_depth);
        for (depth, node) in path.iter().copied().enumerate() {
            let node_key = (status, node.to_path_buf());
            if depth >= shared_depth {
                let entry = if node == comment.path {
                    CommentEntry::File(status, node.into())
                } else {
                    CommentEntry::Folder(status, node.into())
                };
                rows.push(CommentRow {
                    depth: depth + 1,
                    entry,
                });
                previous_path.push(node.to_path_buf());
            }
            if collapsed.contains(&node_key) {
                break;
            }
            if node == comment.path {
                rows.push(CommentRow {
                    depth: depth + 2,
                    entry: CommentEntry::Comment(comment),
                });
                break;
            }
        }
    }
}

struct LineCommentsPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    selection: Entity<CommentStoreSelection>,
    store: Option<(Entity<CommentStore>, Subscription)>,
    filter: Entity<Editor>,
    focus: FocusHandle,
    layout: CommentLayout,
    collapsed: CollapsedCommentTree,
    status_filter: Option<CommentStatus>,
    position: DockPosition,
    rows: Vec<CommentRow>,
    comment_list: ListState,
}

impl LineCommentsPanel {
    fn new(
        workspace: WeakEntity<Workspace>,
        workspace_id: Option<WorkspaceId>,
        project: Entity<Project>,
        selection: Entity<CommentStoreSelection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search line comments…", window, cx);
            editor
        });
        cx.subscribe_in(&filter, window, |panel: &mut Self, _, event, _, cx| {
            if matches!(event, EditorEvent::BufferEdited) {
                panel.refresh_comments(cx);
            }
        })
        .detach();
        cx.observe_in(&selection, window, |panel, _, window, cx| {
            panel.sync_comment_store(None, window, cx);
        })
        .detach();
        let panel = Self {
            workspace,
            project,
            selection,
            store: None,
            filter,
            focus: cx.focus_handle(),
            layout: CommentLayout::default(),
            collapsed: CollapsedCommentTree::default(),
            status_filter: None,
            position: DockPosition::Right,
            rows: Vec::new(),
            comment_list: ListState::new(0, ListAlignment::Top, px(500.)),
        };
        cx.defer_in(window, move |panel, window, cx| {
            panel.sync_comment_store(workspace_id, window, cx);
        });
        panel
    }

    fn sync_comment_store(
        &mut self,
        workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let workspace = workspace.read(cx);
        let Some(workspace_id) = workspace_id.or_else(|| workspace.database_id()) else {
            return;
        };
        let store = workspace_comment_store(workspace_id, &self.selection, &self.project, cx);
        if self
            .store
            .as_ref()
            .is_some_and(|(current, _)| current == &store)
        {
            return;
        }
        let subscription = cx.observe_in(&store, window, |panel, _, _, cx| {
            panel.refresh_comments(cx);
        });
        self.store = Some((store, subscription));
        self.refresh_comments(cx);
    }

    fn refresh_comments(&mut self, cx: &mut Context<Self>) {
        let query = self.filter.read(cx).text(cx);
        self.rows = self.store.as_ref().map_or_else(Vec::new, |(store, _)| {
            let mut matches = match_line_comments(store.read(cx), &query, multiline_comment_label);
            if let Some(status) = self.status_filter {
                matches.retain(|comment| CommentStatus::for_comment(comment) == status);
            }
            comment_rows(
                matches,
                self.layout,
                &self.collapsed,
                self.project.read(cx),
                cx,
            )
        });
        self.comment_list.reset(self.rows.len());
        cx.notify();
    }

    fn render_view_options(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = cx.weak_entity();
        let layout = self.layout;
        let status_filter = self.status_filter;
        PopoverMenu::new("comments-view-options")
            .trigger_with_tooltip(
                IconButton::new("comments-view-options-trigger", IconName::Filter)
                    .icon_size(IconSize::Small),
                Tooltip::text("View Options"),
            )
            .menu(move |window, cx| {
                Some(ContextMenu::build(window, cx, |menu, _, _| {
                    let menu = CommentLayout::OPTIONS.iter().fold(
                        menu.header("View"),
                        |menu, &(option, label)| {
                            let panel = panel.clone();
                            menu.item(
                                ContextMenuEntry::new(label)
                                    .toggle(IconPosition::End, layout == option)
                                    .handler(move |_, cx| {
                                        panel
                                            .update(cx, |panel, cx| {
                                                panel.layout = option;
                                                panel.refresh_comments(cx);
                                            })
                                            .ok();
                                    }),
                            )
                        },
                    );
                    std::iter::once((None, "All Comments"))
                        .chain(CommentStatus::ALL.map(|status| (Some(status), status.label())))
                        .fold(
                            menu.separator().header("Status Filter"),
                            |menu, (status, label)| {
                                let panel = panel.clone();
                                menu.item(
                                    ContextMenuEntry::new(label)
                                        .toggle(IconPosition::End, status_filter == status)
                                        .handler(move |_, cx| {
                                            panel
                                                .update(cx, |panel, cx| {
                                                    panel.status_filter = status;
                                                    panel.refresh_comments(cx);
                                                })
                                                .ok();
                                        }),
                                )
                            },
                        )
                }))
            })
            .anchor(gpui::Anchor::TopRight)
    }

    fn render_comment_row(
        &self,
        index: usize,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let row = self.rows.get(index)?;
        let item = ListItem::new(index)
            .indent_level(row.depth)
            .spacing(ListItemSpacing::Sparse);
        let item = match &row.entry {
            CommentEntry::Status(status, count) => {
                let status = *status;
                let expanded = !self.collapsed.sections.contains(&status);
                item.height(rems(1.75))
                    .start_slot::<Icon>(Some(comment_disclosure(expanded)))
                    .child(Label::new(status.label()).size(LabelSize::Small))
                    .end_slot::<Label>(Some(
                        Label::new(count.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ))
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        if !panel.collapsed.sections.remove(&status) {
                            panel.collapsed.sections.insert(status);
                        }
                        panel.refresh_comments(cx);
                    }))
            }
            CommentEntry::Folder(status, path) => {
                self.render_comment_node(item, *status, path, true, cx)
            }
            CommentEntry::File(status, path) => {
                self.render_comment_node(item, *status, path, false, cx)
            }
            CommentEntry::Comment(comment) => {
                let path = comment.path.clone();
                let line = comment.line;
                let location = match self.layout {
                    CommentLayout::Grouped => line.to_string(),
                    CommentLayout::Flat => format!("{}:{line}", comment_node_name(&path)),
                };
                item.child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .items_start()
                        .gap_2()
                        .py_0p5()
                        .child(self.render_comment_done(comment, cx))
                        .child(
                            div().flex_1().min_w_0().child(
                                comment
                                    .comment_first_line()
                                    .truncate()
                                    .buffer_font(cx)
                                    .size(LabelSize::Custom(
                                        ui::utils::buffer_text_style(cx)
                                            .font_size
                                            .to_rems(window.rem_size()),
                                    )),
                            ),
                        )
                        .child(
                            Label::new(location)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate_start(),
                        ),
                )
                .on_click(cx.listener(move |panel, _, window, cx| {
                    open_comment_location(
                        panel.workspace.clone(),
                        path.clone(),
                        line,
                        true,
                        window,
                        cx,
                    );
                }))
            }
        };
        Some(div().w_full().min_w_0().overflow_hidden().child(item))
    }

    fn render_comment_done(
        &self,
        comment: &LineCommentMatch,
        cx: &mut Context<Self>,
    ) -> IconButton {
        let (icon, label) = comment_done_icon(comment.done);
        let path = comment.path.to_string_lossy().into_owned();
        let line = comment.line;
        let done = !comment.done;
        IconButton::new("comment-done", icon)
            .icon_size(IconSize::Small)
            .size(ButtonSize::Compact)
            .aria_label(label)
            .tooltip(Tooltip::text(label))
            .on_click(cx.listener(move |panel, _, window, cx| {
                cx.stop_propagation();
                let Some((store, _)) = &panel.store else {
                    return;
                };
                let save = store.update(cx, |store, cx| {
                    store.set_comment_done(&path, line, done, cx)
                });
                save.detach_and_notify_err(panel.workspace.clone(), window, cx);
            }))
    }

    fn render_comment_node(
        &self,
        item: ListItem,
        status: CommentStatus,
        path: &Path,
        folder: bool,
        cx: &mut Context<Self>,
    ) -> ListItem {
        let node = (status, path.to_path_buf());
        let expanded = !self.collapsed.paths.contains(&node);
        let (icon_path, fallback) = if folder {
            let fallback = if expanded {
                IconName::FolderOpen
            } else {
                IconName::Folder
            };
            (FileIcons::get_folder_icon(expanded, path, cx), fallback)
        } else {
            (FileIcons::get_icon(path, cx), IconName::File)
        };
        let icon = icon_path.map_or_else(|| Icon::new(fallback), Icon::from_path);
        let name = comment_node_name(path).into_owned();
        item.height(rems(1.5))
            .start_slot::<Div>(Some(
                h_flex()
                    .gap_1()
                    .child(comment_disclosure(expanded))
                    .child(icon.size(IconSize::Small)),
            ))
            .child(Label::new(name).truncate())
            .on_click(cx.listener(move |panel, _, _, cx| {
                if !panel.collapsed.paths.remove(&node) {
                    panel.collapsed.paths.insert(node.clone());
                }
                panel.refresh_comments(cx);
            }))
    }
}

fn comment_disclosure(expanded: bool) -> Icon {
    Icon::new(if expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    })
    .size(IconSize::Small)
    .color(Color::Muted)
}

fn comment_node_name(path: &Path) -> std::borrow::Cow<'_, str> {
    path.file_name()
        .map_or("Workspace".into(), |name| name.to_string_lossy())
}

impl Focusable for LineCommentsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl EventEmitter<PanelEvent> for LineCommentsPanel {}

impl Panel for LineCommentsPanel {
    fn persistent_name() -> &'static str {
        "Comments"
    }

    fn panel_key() -> &'static str {
        "line_comments"
    }

    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.filter.focus_handle(cx)
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Chat)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Comments Panel")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        7
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        if active {
            cx.defer_in(window, |panel, window, cx| {
                panel.sync_comment_store(None, window, cx)
            });
        }
    }
}

impl Render for LineCommentsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .track_focus(&self.focus)
            .size_full()
            .min_h_0()
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1().min_w_0().child(self.filter.clone()))
                    .child(self.render_view_options(cx)),
            )
            .child(Divider::horizontal())
            .when(self.rows.is_empty(), |panel| {
                panel.child(
                    div()
                        .p_2()
                        .child(Label::new("No matching comments").color(Color::Muted)),
                )
            })
            .child(
                list(
                    self.comment_list.clone(),
                    cx.processor(|panel, index, window, cx| {
                        panel
                            .render_comment_row(index, window, cx)
                            .map(IntoElement::into_any_element)
                            .unwrap_or_else(|| div().into_any_element())
                    }),
                )
                .w_full()
                .flex_1(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use serde_json::json;
    use util::path;

    #[gpui::test]
    async fn test_comments_panel_filters_and_sorts_workspace_comments(cx: &mut TestAppContext) {
        let file = path!("/root/a.rs");
        let (fs, editor, _, mut cx) = crate::line_comments_tests::open_file(
            file,
            Some(json!([
                { "line": 1, "text": "check apple\nsecond line", "done": true },
                { "line": 2, "text": "check zebra" },
                { "line": 3, "text": "other note" }
            ])),
            cx,
        )
        .await;
        fs.insert_tree(path!("/root/src"), json!({ "b.rs": "one\n" }))
            .await;
        let store = editor.read_with(&cx, |editor, _| {
            crate::editor_comment_store(editor).unwrap()
        });
        store
            .update(&mut cx, |store, cx| {
                store.upsert(
                    path!("/root/src/b.rs"),
                    crate::LineComment {
                        line: 1,
                        end_line: None,
                        text: "nested comment".into(),
                        done: false,
                        orphaned: false,
                        author: crate::Author::Human,
                    },
                    cx,
                )
            })
            .await
            .unwrap();
        let workspace = editor.read_with(&cx, |editor, _| editor.workspace().unwrap());
        let panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            add_line_comments_panel(workspace, window, cx);
            workspace
                .focus_panel::<LineCommentsPanel>(window, cx)
                .unwrap()
        });
        cx.run_until_parked();

        panel.read_with(&cx, |panel, _| {
            assert!(matches!(&panel.rows[0].entry, CommentEntry::Status(CommentStatus::Open, 3)));
            assert!(matches!(&panel.rows[1].entry, CommentEntry::Folder(CommentStatus::Open, path) if comment_node_name(path) == "root"));
            assert!(matches!(&panel.rows[2].entry, CommentEntry::File(CommentStatus::Open, path) if comment_node_name(path) == "a.rs"));
            assert_eq!(panel.rows[3].depth, 3);
            assert!(matches!(&panel.rows[8].entry, CommentEntry::Status(CommentStatus::Done, 1)));
            assert!(matches!(&panel.rows[11].entry, CommentEntry::Comment(comment) if comment.done && comment.comment_first_line().text() == "check apple"));
            let multiline = panel.comment_list.bounds_for_item(11).unwrap();
            let single_line = panel.comment_list.bounds_for_item(3).unwrap();
            assert_eq!(multiline.size.height, single_line.size.height);
            assert!(matches!(&panel.rows[5].entry, CommentEntry::Folder(CommentStatus::Open, path) if comment_node_name(path) == "src"));
            assert!(matches!(&panel.rows[6].entry, CommentEntry::File(CommentStatus::Open, path) if comment_node_name(path) == "b.rs"));
            assert_eq!(panel.rows[7].depth, 4);
        });
        panel.update(&mut cx, |panel, cx| {
            panel
                .collapsed
                .paths
                .insert((CommentStatus::Open, file.into()));
            panel.refresh_comments(cx);
            assert_eq!(panel.rows.len(), 10);
            assert!(matches!(&panel.rows[9].entry, CommentEntry::Comment(comment) if comment.done));
            panel.layout = CommentLayout::Flat;
            panel.refresh_comments(cx);
        });
        cx.simulate_input("check");
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| {
            assert!(matches!(&panel.rows[0].entry, CommentEntry::Status(CommentStatus::Open, 1)));
            assert!(matches!(&panel.rows[1].entry, CommentEntry::Comment(comment) if comment.text == "check zebra"));
            assert!(matches!(&panel.rows[2].entry, CommentEntry::Status(CommentStatus::Done, 1)));
            assert!(matches!(&panel.rows[3].entry, CommentEntry::Comment(comment) if comment.text == "check apple\nsecond line"));
        });
        panel.update(&mut cx, |panel, cx| {
            panel.status_filter = Some(CommentStatus::Done);
            panel.refresh_comments(cx);
            assert_eq!(panel.rows.len(), 2);
            assert!(matches!(&panel.rows[1].entry, CommentEntry::Comment(comment) if comment.done));
        });
        cx.dispatch_action(editor::actions::SelectAll);
        cx.simulate_input("second line");
        cx.run_until_parked();
        panel.read_with(&cx, |panel, _| {
            assert_eq!(panel.rows.len(), 2);
            assert!(matches!(&panel.rows[1].entry, CommentEntry::Comment(comment) if comment.comment_first_line().text() == "check apple"));
        });
    }
}
