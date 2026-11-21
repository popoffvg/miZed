mod comment_tools;
mod file_handoff;
#[cfg(test)]
mod mized_server_tests;
mod workspace_switch;

use std::{io::Write as _, path::Path};

use anyhow::{Context as _, Result, ensure};
use context_server::{
    listener::{McpServer, ToolResponse},
    protocol::supports_protocol_version,
    types::{
        Implementation, InitializeResponse, LATEST_PROTOCOL_VERSION, ProtocolVersion, Request,
        ServerCapabilities, ToolResponseContent, ToolsCapabilities, requests::Initialize,
    },
};
use gpui::{App, AppContext as _, Entity, Global, Task, TaskExt as _, WindowHandle};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use workspace::{MultiWorkspace, Workspace};

struct MizedMcpServer {
    _server: McpServer,
}

impl Global for MizedMcpServer {}

#[derive(Deserialize, JsonSchema)]
struct WorkspaceTarget {
    /// ID of an open miZed workspace. Defaults to the active local workspace.
    workspace_id: Option<i64>,
}

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
            let protocol_version = if supports_protocol_version(&request.protocol_version) {
                request.protocol_version
            } else {
                ProtocolVersion(LATEST_PROTOCOL_VERSION.to_owned())
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
        server.add_tool(comment_tools::AddComment);
        server.add_tool(comment_tools::ReadComment);
        server.add_tool(comment_tools::ListComments);
        server.add_tool(comment_tools::ClearComments);
        server.add_tool(file_handoff::OpenFile);
        server.add_tool(file_handoff::OpenMultibuffer);
        server.add_tool(file_handoff::OpenDraft);
        server.add_tool(workspace_switch::ListWorkspaces);
        server.add_tool(workspace_switch::SwitchWorkspace);
        server.add_tool(workspace_switch::ListRepositories);
        server.add_tool(workspace_switch::SwitchRepository);
        server.add_tool(workspace_switch::ListBranches);
        server.add_tool(workspace_switch::SwitchBranch);

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

    let mut candidates = local_workspaces(cx).filter(|(_, workspace)| {
        workspace_id.is_none_or(|id| workspace.read(cx).database_id().map(i64::from) == Some(id))
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

fn local_workspaces(
    cx: &App,
) -> impl Iterator<Item = (WindowHandle<MultiWorkspace>, &Entity<Workspace>)> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .filter_map(|window| window.read(cx).ok().map(|root| (window, root)))
        .flat_map(|(window, root)| root.workspaces().map(move |workspace| (window, workspace)))
        .filter(|(_, workspace)| workspace.read(cx).project().read(cx).is_local())
}

fn mcp_response<T: Serialize>(value: T) -> Result<ToolResponse<T>> {
    Ok(ToolResponse {
        content: vec![ToolResponseContent::Text {
            text: serde_json::to_string(&value)?,
        }],
        structured_content: value,
    })
}
