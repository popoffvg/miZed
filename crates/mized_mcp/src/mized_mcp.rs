#[cfg(unix)]
mod mized_server;

#[cfg(unix)]
pub fn start_mized_mcp(cx: &mut gpui::App) {
    mized_server::start_mized_server(cx);
}

#[cfg(not(unix))]
pub fn start_mized_mcp(_: &mut gpui::App) {}
