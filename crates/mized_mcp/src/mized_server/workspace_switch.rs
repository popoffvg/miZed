use std::path::PathBuf;

use anyhow::{Context as _, Result, ensure};
use context_server::{
    listener::{McpServerTool, ToolResponse},
    types::ToolAnnotations,
};
use gpui::{App, AsyncApp, Entity, WindowHandle};
use project::git_store::Repository;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use workspace::{MultiWorkspace, Workspace};

use super::{WorkspaceTarget, local_workspaces, mcp_response, mized_workspace};

#[derive(Clone)]
pub(super) struct ListWorkspaces;
#[derive(Clone)]
pub(super) struct SwitchWorkspace;
#[derive(Clone)]
pub(super) struct ListRepositories;
#[derive(Clone)]
pub(super) struct SwitchRepository;
#[derive(Clone)]
pub(super) struct ListBranches;
#[derive(Clone)]
pub(super) struct SwitchBranch;

/// List the open local miZed workspaces.
#[derive(Deserialize, JsonSchema)]
pub(super) struct ListWorkspacesInput {}

/// Switch to an open local miZed workspace.
#[derive(Deserialize, JsonSchema)]
pub(super) struct SwitchWorkspaceInput {
    /// Workspace ID from list_workspaces.
    workspace_id: i64,
}

/// List Git repositories in a local miZed workspace.
#[derive(Deserialize, JsonSchema)]
pub(super) struct ListRepositoriesInput {
    #[serde(flatten)]
    workspace: WorkspaceTarget,
}

/// Select the Git repository used by miZed's Git tools.
#[derive(Deserialize, JsonSchema)]
pub(super) struct SwitchRepositoryInput {
    #[serde(flatten)]
    workspace: WorkspaceTarget,
    /// Absolute worktree path from list_repositories.
    path: PathBuf,
}

/// List local branches of the selected Git repository.
#[derive(Deserialize, JsonSchema)]
pub(super) struct ListBranchesInput {
    #[serde(flatten)]
    workspace: WorkspaceTarget,
}

/// Switch the selected Git repository to an existing local branch.
#[derive(Deserialize, JsonSchema)]
pub(super) struct SwitchBranchInput {
    #[serde(flatten)]
    workspace: WorkspaceTarget,
    /// Local branch name from list_branches.
    branch: String,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct WorkspaceList {
    workspaces: Vec<WorkspaceSummary>,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct WorkspaceSummary {
    workspace_id: Option<i64>,
    root_paths: Vec<PathBuf>,
    /// Whether this workspace is displayed in its window.
    active: bool,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct RepositoryList {
    repositories: Vec<RepositorySummary>,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct RepositorySummary {
    path: PathBuf,
    branch: Option<String>,
    active: bool,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct BranchList {
    path: PathBuf,
    branches: Vec<BranchSummary>,
}

#[derive(Serialize, JsonSchema)]
struct BranchSummary {
    name: String,
    current: bool,
}

#[derive(Serialize, JsonSchema)]
pub(super) struct SwitchedBranch {
    path: PathBuf,
    branch: String,
}

impl McpServerTool for ListWorkspaces {
    const NAME: &'static str = "list_workspaces";
    type Input = ListWorkspacesInput;
    type Output = WorkspaceList;

    fn annotations(&self) -> ToolAnnotations {
        read_only_annotations()
    }

    async fn run(&self, _: Self::Input, cx: &mut AsyncApp) -> Result<ToolResponse<Self::Output>> {
        let workspaces = cx.update(|cx| {
            let mut workspaces: Vec<_> = local_workspaces(cx)
                .map(|(window, workspace)| {
                    let active = window
                        .read(cx)
                        .is_ok_and(|root| root.workspace() == workspace);
                    workspace_summary(workspace.read(cx), active, cx)
                })
                .collect();
            workspaces.sort_by(|left, right| {
                left.root_paths
                    .cmp(&right.root_paths)
                    .then(left.workspace_id.cmp(&right.workspace_id))
            });
            workspaces
        });
        mcp_response(WorkspaceList { workspaces })
    }
}

impl McpServerTool for SwitchWorkspace {
    const NAME: &'static str = "switch_workspace";
    type Input = SwitchWorkspaceInput;
    type Output = WorkspaceSummary;

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let (window, workspace) = cx.update(|cx| mized_workspace(Some(input.workspace_id), cx))?;
        activate_workspace(window, &workspace, cx)?;
        mcp_response(
            workspace.read_with(cx, |workspace, cx| workspace_summary(workspace, true, cx)),
        )
    }
}

impl McpServerTool for ListRepositories {
    const NAME: &'static str = "list_repositories";
    type Input = ListRepositoriesInput;
    type Output = RepositoryList;

    fn annotations(&self) -> ToolAnnotations {
        read_only_annotations()
    }

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let (_, workspace) = cx.update(|cx| mized_workspace(input.workspace.workspace_id, cx))?;
        let repositories = workspace.read_with(cx, |workspace, cx| {
            let project = workspace.project().read(cx);
            let active = project.active_repository(cx);
            let mut repositories: Vec<_> = project
                .repositories(cx)
                .values()
                .map(|repository| {
                    repository_summary(repository.read(cx), active.as_ref() == Some(repository))
                })
                .collect();
            repositories.sort_by(|left, right| left.path.cmp(&right.path));
            repositories
        });
        mcp_response(RepositoryList { repositories })
    }
}

impl McpServerTool for SwitchRepository {
    const NAME: &'static str = "switch_repository";
    type Input = SwitchRepositoryInput;
    type Output = RepositorySummary;

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        ensure!(
            input.path.is_absolute(),
            "Use an absolute Git worktree path"
        );
        let (window, workspace) =
            cx.update(|cx| mized_workspace(input.workspace.workspace_id, cx))?;
        let repository = workspace.read_with(cx, |workspace, cx| {
            workspace
                .project()
                .read(cx)
                .repositories(cx)
                .values()
                .find(|repository| {
                    repository.read(cx).work_directory_abs_path.as_ref() == input.path
                })
                .cloned()
                .with_context(|| {
                    format!("miZed has no open repository at {}", input.path.display())
                })
        })?;
        activate_workspace(window, &workspace, cx)?;
        let summary = repository.update(cx, |repository, cx| {
            repository.set_as_active_repository(cx);
            repository_summary(repository, true)
        });
        mcp_response(summary)
    }
}

impl McpServerTool for ListBranches {
    const NAME: &'static str = "list_branches";
    type Input = ListBranchesInput;
    type Output = BranchList;

    fn annotations(&self) -> ToolAnnotations {
        read_only_annotations()
    }

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let repository = cx.update(|cx| active_repository(input.workspace, cx))?;
        let path = repository.read_with(cx, |repository, _| {
            repository.work_directory_abs_path.to_path_buf()
        });
        let branches = local_branches(&repository, cx).await?;
        mcp_response(BranchList { path, branches })
    }
}

impl McpServerTool for SwitchBranch {
    const NAME: &'static str = "switch_branch";
    type Input = SwitchBranchInput;
    type Output = SwitchedBranch;

    async fn run(
        &self,
        input: Self::Input,
        cx: &mut AsyncApp,
    ) -> Result<ToolResponse<Self::Output>> {
        let repository = cx.update(|cx| active_repository(input.workspace, cx))?;
        let branches = local_branches(&repository, cx).await?;
        ensure!(
            !input.branch.starts_with('-')
                && branches.iter().any(|branch| branch.name == input.branch),
            "miZed found no local branch named {}",
            input.branch
        );
        let path = repository.read_with(cx, |repository, _| {
            repository.work_directory_abs_path.to_path_buf()
        });
        repository
            .update(cx, |repository, _| {
                repository.change_branch(input.branch.clone())
            })
            .await
            .context("miZed did not complete the branch switch")??;
        mcp_response(SwitchedBranch {
            path,
            branch: input.branch,
        })
    }
}

fn activate_workspace(
    window: WindowHandle<MultiWorkspace>,
    workspace: &Entity<Workspace>,
    cx: &mut AsyncApp,
) -> Result<()> {
    window.update(cx, |root, window, cx| {
        if root.workspace() != workspace {
            root.activate(workspace.clone(), None, window, cx);
        }
    })
}

fn workspace_summary(workspace: &Workspace, active: bool, cx: &App) -> WorkspaceSummary {
    WorkspaceSummary {
        workspace_id: workspace.database_id().map(i64::from),
        root_paths: workspace
            .root_paths(cx)
            .iter()
            .map(|path| path.to_path_buf())
            .collect(),
        active,
    }
}

fn repository_summary(repository: &Repository, active: bool) -> RepositorySummary {
    RepositorySummary {
        path: repository.work_directory_abs_path.to_path_buf(),
        branch: repository
            .branch
            .as_ref()
            .map(|branch| branch.name().to_owned()),
        active,
    }
}

fn active_repository(target: WorkspaceTarget, cx: &App) -> Result<Entity<Repository>> {
    let (_, workspace) = mized_workspace(target.workspace_id, cx)?;
    workspace
        .read(cx)
        .project()
        .read(cx)
        .active_repository(cx)
        .context("The miZed workspace has no selected Git repository")
}

async fn local_branches(
    repository: &Entity<Repository>,
    cx: &mut AsyncApp,
) -> Result<Vec<BranchSummary>> {
    let scan = repository
        .update(cx, |repository, _| repository.branches())
        .await
        .context("miZed did not complete the branch scan")??;
    ensure!(
        scan.error.is_none(),
        "Could not list miZed branches: {}",
        scan.error.unwrap_or_default()
    );
    let mut branches: Vec<_> = scan
        .branches
        .iter()
        .filter(|branch| !branch.is_remote())
        .map(|branch| BranchSummary {
            name: branch.name().to_owned(),
            current: branch.is_head,
        })
        .collect();
    branches.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(branches)
}

fn read_only_annotations() -> ToolAnnotations {
    ToolAnnotations {
        title: None,
        read_only_hint: Some(true),
        destructive_hint: Some(false),
        idempotent_hint: Some(true),
        open_world_hint: Some(false),
    }
}
