mod comment_tools;
mod file_handoff;

use std::{io::Write as _, path::Path};

use anyhow::{Context as _, Result, ensure};
use context_server::{
    listener::{McpServer, ToolResponse},
    types::{
        Implementation, InitializeResponse, LATEST_PROTOCOL_VERSION, ProtocolVersion, Request,
        ServerCapabilities, ToolResponseContent, ToolsCapabilities, VERSION_2024_11_05,
        VERSION_2025_03_26, VERSION_2025_06_18, requests::Initialize,
    },
};
use gpui::{App, AppContext as _, Entity, Global, Task, TaskExt as _, WindowHandle};
use serde::Serialize;
use serde_json::{Value, json};
use workspace::{MultiWorkspace, Workspace};

struct MizedMcpServer {
    _server: McpServer,
}

impl Global for MizedMcpServer {}

struct McpPing;

impl Request for McpPing {
    type Params = Option<Value>;
    type Response = Value;
    const METHOD: &'static str = "ping";
}

pub(super) fn start_mized_server(cx: &mut App) {
    let Some(address_file) = std::env::var_os("MIZED_MCP_ADDRESS_FILE") else {
        return;
    };
    cx.spawn(async move |cx| {
        let mut server = McpServer::new(cx).await?;
        server.handle_request::<Initialize>(|request, _| {
            let protocol_version = match request.protocol_version.0.as_str() {
                VERSION_2024_11_05
                | VERSION_2025_03_26
                | VERSION_2025_06_18
                | LATEST_PROTOCOL_VERSION => request.protocol_version,
                _ => ProtocolVersion(LATEST_PROTOCOL_VERSION.to_owned()),
            };
            Task::ready(Ok(InitializeResponse {
                protocol_version,
                capabilities: ServerCapabilities {
                    tools: Some(ToolsCapabilities { list_changed: None }),
                    ..Default::default()
                },
                server_info: Implementation {
                    name: "miZed".to_owned(),
                    title: None,
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    description: None,
                },
                meta: None,
            }))
        });
        server.handle_request::<McpPing>(|_, _| Task::ready(Ok(json!({}))));
        server.add_tool(comment_tools::AddComment::default());
        server.add_tool(comment_tools::ReadComment);
        server.add_tool(comment_tools::ListComments);
        server.add_tool(file_handoff::OpenFile);
        server.add_tool(file_handoff::OpenMultibuffer);
        server.add_tool(file_handoff::OpenDraft);

        let socket_path = server.socket_path().to_owned();
        cx.background_spawn(
            async move { publish_mcp_address(Path::new(&address_file), &socket_path) },
        )
        .await?;
        cx.update(|cx| cx.set_global(MizedMcpServer { _server: server }));
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

fn publish_mcp_address(address_file: &Path, socket_path: &Path) -> Result<()> {
    ensure!(
        address_file.is_absolute(),
        "Use an absolute miZed MCP address path"
    );
    let address_folder = address_file
        .parent()
        .context("The miZed MCP address needs a parent folder")?;
    let mut file = tempfile::NamedTempFile::new_in(address_folder)
        .context("Could not create the miZed MCP address file")?;
    file.write_all(socket_path.as_os_str().as_encoded_bytes())?;
    file.persist(address_file)
        .context("Could not publish the miZed MCP address file")?;
    Ok(())
}

fn mized_workspace(
    workspace_id: Option<i64>,
    cx: &App,
) -> Result<(WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
    if workspace_id.is_none()
        && let Some(window) = cx
            .active_window()
            .and_then(|window| window.downcast::<MultiWorkspace>())
        && let Ok(root) = window.read(cx)
        && root.workspace().read(cx).project().read(cx).is_local()
    {
        return Ok((window, root.workspace().clone()));
    }

    let mut candidates = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .filter_map(|window| window.read(cx).ok().map(|root| (window, root)))
        .flat_map(|(window, root)| root.workspaces().map(move |workspace| (window, workspace)))
        .filter(|(_, workspace)| {
            let workspace = workspace.read(cx);
            workspace.project().read(cx).is_local()
                && workspace_id.is_none_or(|id| workspace.database_id().map(i64::from) == Some(id))
        });
    let (window, workspace) = candidates
        .next()
        .context("No matching local miZed workspace is open")?;
    ensure!(
        candidates.next().is_none(),
        "Select a miZed workspace with workspace_id"
    );
    Ok((window, workspace.clone()))
}

fn mcp_response<T: Serialize>(value: T) -> Result<ToolResponse<T>> {
    Ok(ToolResponse {
        content: vec![ToolResponseContent::Text {
            text: serde_json::to_string(&value)?,
        }],
        structured_content: value,
    })
}
