use gpui::{App, Div, Styled, div};
use theme::ActiveTheme;

use crate::StyledExt;

/// Horizontally stacks elements. Sets `flex()`, `flex_row()`, `items_center()`
#[track_caller]
pub fn h_flex() -> Div {
    div().h_flex()
}

/// Vertically stacks elements. Sets `flex()`, `flex_col()`
#[track_caller]
pub fn v_flex() -> Div {
    div().v_flex()
}

pub fn inline_prompt_frame(cx: &App) -> Div {
    v_flex()
        .size_full()
        .pt_0p5()
        .gap_0p5()
        .justify_center()
        .border_y_1()
        .border_color(cx.theme().colors().border)
        .bg(cx.theme().colors().editor_background)
}
