use std::{path::PathBuf, sync::Arc};

use editor::Editor;
use fuzzy_nucleo::{StringMatchCandidate, match_strings};
use gpui::{App, Context, DismissEvent, Entity, Task, WeakEntity, Window};
use language::Point;
use picker::{Picker, PickerDelegate, PreviewUpdate};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use workspace::{Workspace, notifications::NotifyTaskExt as _};

use crate::{
    CommentStore, LineComment, ListLineComments, ProjectWorkspaces, comment_label,
    workspace_comment_store,
};

pub(crate) fn list_line_comments(
    workspace: &mut Workspace,
    _: &ListLineComments,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    if !project.read(cx).is_local() {
        return;
    }
    let Some(workspace_id) = workspace.database_id() else {
        return;
    };
    let Some(selection) = cx
        .try_global::<ProjectWorkspaces>()
        .and_then(|workspaces| workspaces.0.get(&project.entity_id()))
        .map(|stores| stores.selection.clone())
    else {
        return;
    };
    let store = workspace_comment_store(workspace_id, &selection, &project, cx);
    let workspace_handle = cx.weak_entity();
    workspace.toggle_modal(window, cx, move |window, cx| {
        cx.observe_in(
            &store,
            window,
            |picker: &mut Picker<LineCommentPicker>, _, window, cx| {
                picker.refresh(window, cx);
            },
        )
        .detach();
        let delegate = LineCommentPicker {
            workspace: workspace_handle,
            store,
            matches: Vec::new(),
            selected_index: 0,
        };
        let preview = picker_preview::editor_preview(project, window, cx);
        Picker::uniform_list_with_preview(delegate, preview, window, cx).reopenable(false, cx)
    });
}

pub(crate) struct LineCommentMatch {
    pub(crate) path: PathBuf,
    pub(crate) line: u32,
    pub(crate) text: SharedString,
    pub(crate) done: bool,
    label: SharedString,
    positions: Vec<usize>,
}

pub(crate) fn match_line_comments(
    store: &CommentStore,
    query: &str,
    label: fn(&LineComment) -> SharedString,
) -> Vec<LineCommentMatch> {
    let locations: Vec<_> = store.all_comments().collect();
    let candidates: Vec<_> = locations
        .iter()
        .enumerate()
        .map(|(index, (_, comment))| StringMatchCandidate::new(index, label(comment)))
        .collect();
    match_strings(&candidates, query, Default::default())
        .into_iter()
        .filter_map(|matched| {
            let &(path, comment) = locations.get(matched.candidate_id)?;
            Some(LineCommentMatch {
                path: path.into(),
                line: comment.line,
                text: comment.text.as_str().into(),
                done: comment.done,
                label: matched.string,
                positions: matched.positions,
            })
        })
        .collect()
}

impl LineCommentMatch {
    fn highlighted_comment(&self) -> HighlightedLabel {
        HighlightedLabel::new(self.label.clone(), self.positions.clone())
    }

    pub(crate) fn comment_first_line(&self) -> HighlightedLabel {
        let first_line = self.label.lines().next().unwrap_or_default();
        let positions = self
            .positions
            .iter()
            .copied()
            .filter(|position| *position < first_line.len())
            .collect();
        HighlightedLabel::new(first_line, positions)
    }

    pub(crate) fn render_comment_match(&self, index: usize, selected: bool) -> ListItem {
        ListItem::new(index)
            .spacing(ListItemSpacing::Sparse)
            .inset(true)
            .toggle_state(selected)
            .start_slot::<Icon>(Some(Icon::new(IconName::Chat).color(Color::Info)))
            .child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .child(self.highlighted_comment().truncate())
                    .child(
                        Label::new(format!("{}:{}", self.path.display(), self.line))
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate_start(),
                    ),
            )
    }
}

pub(crate) fn open_comment_location<T: 'static>(
    workspace: WeakEntity<Workspace>,
    path: PathBuf,
    line: u32,
    focus: bool,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let point = Point::new(line.saturating_sub(1), 0);
    let workspace_handle = workspace.clone();
    let task = cx.spawn_in(window, async move |_, cx| {
        let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
        let buffer = project
            .update(cx, |project, cx| project.open_local_buffer(path, cx))
            .await?;
        let editor = workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_project_item::<Editor>(None, buffer, true, true, true, focus, window, cx)
        })?;
        editor.update_in(cx, |editor, window, cx| {
            editor.go_to_singleton_buffer_point(point, window, cx);
        })?;
        anyhow::Ok(())
    });
    task.detach_and_notify_err(workspace_handle, window, cx);
}

struct LineCommentPicker {
    workspace: WeakEntity<Workspace>,
    store: Entity<CommentStore>,
    matches: Vec<LineCommentMatch>,
    selected_index: usize,
}

impl PickerDelegate for LineCommentPicker {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "line comments"
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Search for a line comment…".into()
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.matches = match_line_comments(self.store.read(cx), &query, comment_label);
        self.selected_index = 0;
        Task::ready(())
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(selected) = self.matches.get(self.selected_index) else {
            return;
        };
        cx.emit(DismissEvent);
        open_comment_location(
            self.workspace.clone(),
            selected.path.clone(),
            selected.line,
            !secondary,
            window,
            cx,
        );
    }

    fn dismissed(&mut self, _: &mut Window, _: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<ListItem> {
        let comment = self.matches.get(index)?;
        Some(comment.render_comment_match(index, selected))
    }

    fn try_get_preview_data_for_match(&self, _: &App) -> Option<PreviewUpdate> {
        let comment = self.matches.get(self.selected_index)?;
        Some(PreviewUpdate::from_path(comment.path.clone()))
    }
}
