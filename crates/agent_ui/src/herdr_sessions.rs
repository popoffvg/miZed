use std::path::{Path, PathBuf};

use anyhow::Context as _;
use chrono::DateTime;
use collections::HashMap;
use project::WorktreePaths;
use serde::Deserialize;
use workspace::{PathList, WorkspaceId};

use crate::{TerminalId, terminal_thread_metadata_store::TerminalThreadMetadata};

#[derive(Debug, PartialEq)]
pub(crate) struct HerdrSession {
    pub name: String,
    pub metadata: TerminalThreadMetadata,
    pub status: Option<HerdrThreadStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HerdrThreadStatus {
    Idle,
    Working,
    Done,
    NeedsAttention,
}

#[derive(Deserialize)]
struct DesktopStatus {
    app: Option<String>,
    state: DesktopStatusState,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum DesktopStatusState {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct DesktopTerminal {
    address: String,
    labels: HashMap<String, String>,
    cwd: PathBuf,
    last_cwd: Option<PathBuf>,
    title: Option<String>,
    name: Option<String>,
    created_ms: i64,
    state: String,
    status: Option<DesktopStatus>,
    turn: Option<u64>,
}

impl DesktopTerminal {
    fn thread_status(&self) -> Option<HerdrThreadStatus> {
        let status = self.status.as_ref()?;
        let state = &status.state;
        match state {
            DesktopStatusState::Idle | DesktopStatusState::Done => {
                if status.app.as_deref() == Some("claude-code")
                    && self.labels.get("zed.claude_triage").map(String::as_str) == Some("question")
                {
                    return Some(HerdrThreadStatus::NeedsAttention);
                }
                let seen = self
                    .labels
                    .get("herdr.seen")
                    .and_then(|seen| seen.parse::<u64>().ok())
                    .unwrap_or_default();
                let unseen_turn = self
                    .turn
                    .map_or(matches!(state, DesktopStatusState::Done), |turn| {
                        turn > seen
                    });
                Some(if unseen_turn {
                    HerdrThreadStatus::Done
                } else {
                    HerdrThreadStatus::Idle
                })
            }
            DesktopStatusState::Working => Some(HerdrThreadStatus::Working),
            DesktopStatusState::Blocked | DesktopStatusState::Error => {
                Some(HerdrThreadStatus::NeedsAttention)
            }
            DesktopStatusState::Unknown => None,
        }
    }
}

#[derive(Deserialize)]
struct CoreSnapshot {
    terminals: HashMap<String, DesktopTerminal>,
}

fn core_directory() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home_dir().join(".local/state"))
        .join("herdr-core/default")
}

fn terminal_address(address: &str) -> anyhow::Result<&str> {
    anyhow::ensure!(address.strip_prefix('t').is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric())), "Invalid Herdr Desktop terminal address");
    Ok(address)
}

pub(crate) async fn read_sessions() -> anyhow::Result<HashMap<TerminalId, HerdrSession>> {
    let path = core_directory().join("terminals.json");
    let contents = match std::fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::default()),
        Err(error) => return Err(error.into()),
    };
    desktop_sessions(&contents)
}

fn desktop_sessions(contents: &[u8]) -> anyhow::Result<HashMap<TerminalId, HerdrSession>> {
    let snapshot: CoreSnapshot =
        serde_json::from_slice(contents).context("Could not read Herdr Desktop terminals")?;
    let mut sessions = HashMap::default();
    for terminal in snapshot.terminals.into_values() {
        if terminal.state != "live"
            || !terminal.labels.contains_key("herdr.space")
            || terminal.labels.contains_key("zed.closed")
        {
            continue;
        }
        let address = terminal_address(&terminal.address)?;
        let id = terminal
            .labels
            .get("zed.thread")
            .and_then(|id| TerminalId::from_key_string(id).ok())
            .unwrap_or_else(|| TerminalId::from_herdr_name(address));
        let status = terminal.thread_status();
        let directory = terminal
            .last_cwd
            .filter(|path| path.is_absolute())
            .unwrap_or(terminal.cwd);
        let title = terminal
            .title
            .filter(|title| !title.trim().is_empty())
            .or(terminal.name)
            .unwrap_or_else(|| address.to_owned());
        sessions.insert(
            id,
            HerdrSession {
                name: address.to_owned(),
                status,
                metadata: TerminalThreadMetadata {
                    terminal_id: id,
                    title: title.into(),
                    custom_title: None,
                    created_at: DateTime::from_timestamp_millis(terminal.created_ms)
                        .context("Invalid Herdr Desktop terminal creation time")?,
                    worktree_paths: WorktreePaths::from_folder_paths(&PathList::new(&[&directory])),
                    remote_connection: None,
                    working_directory: Some(directory),
                },
            },
        );
    }
    Ok(sessions)
}

async fn desktop_command(program: &str, arguments: &[&str]) -> anyhow::Result<Vec<u8>> {
    let output = util::command::new_command(program)
        .env_remove("HC_SOCKET")
        .env_remove("HC_TERMINAL_ID")
        .env("HC_SESSION", "default")
        .args(arguments)
        .output()
        .await
        .with_context(|| format!("Could not start {program}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{program} failed: {} {}",
        String::from_utf8_lossy(&output.stderr).trim(),
        String::from_utf8_lossy(&output.stdout).trim()
    );
    Ok(output.stdout)
}

pub(crate) async fn close_thread(address: &str) -> anyhow::Result<()> {
    desktop_command(
        "hc",
        &["label", terminal_address(address)?, "zed.closed=true"],
    )
    .await?;
    Ok(())
}

pub(crate) async fn attach_program(
    id: TerminalId,
    workspace_id: Option<WorkspaceId>,
    existing: Option<&str>,
    restore: bool,
    directory: Option<&Path>,
) -> anyhow::Result<task::Shell> {
    let address = if let Some(existing) = existing {
        terminal_address(existing)?.to_owned()
    } else {
        anyhow::ensure!(
            !restore,
            "The Herdr Desktop terminal is no longer available"
        );
        let folder = directory.unwrap_or_else(|| paths::home_dir());
        let folder = folder.to_str().context("Invalid Herdr Desktop folder")?;
        let layout: serde_json::Value =
            serde_json::from_slice(&desktop_command("herdr-app", &["ls"]).await?)?;
        let spaces = layout
            .get("spaces")
            .unwrap_or(&layout)
            .as_array()
            .context("Could not read Herdr Desktop spaces")?;
        let space = spaces
            .iter()
            .find(|space| {
                space["folder"].as_str() == Some(folder)
                    && space["machine"].as_str().unwrap_or_default().is_empty()
            })
            .and_then(|space| space["id"].as_str());
        let mut arguments = serde_json::json!({"folder": folder, "focus": false});
        let action = if let Some(space) = space {
            arguments["space"] = space.into();
            "tab.new"
        } else {
            "space.new"
        };
        let reply: serde_json::Value = serde_json::from_slice(
            &desktop_command("herdr-app", &["do", action, &arguments.to_string()]).await?,
        )?;
        let address = terminal_address(
            reply["terminal"]
                .as_str()
                .context("Herdr Desktop did not return a terminal address")?,
        )?;
        let thread_label = format!("zed.thread={id}");
        let workspace_label = workspace_id.map(|id| format!("zed.workspace_id={}", i64::from(id)));
        let mut labels = vec!["label", address, &thread_label];
        labels.extend(workspace_label.as_deref());
        desktop_command("hc", &labels).await?;
        address.to_owned()
    };
    Ok(task::Shell::WithArguments {
        program: "/usr/bin/env".into(),
        args: vec![
            "-u".into(),
            "HC_SOCKET".into(),
            "HC_SESSION=default".into(),
            "hc".into(),
            "attach".into(),
            address,
        ],
        title_override: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_question_needs_attention_after_completion() {
        for (app, state, triage, expected) in [
            (
                "claude-code",
                "done",
                "question",
                HerdrThreadStatus::NeedsAttention,
            ),
            (
                "claude-code",
                "idle",
                "question",
                HerdrThreadStatus::NeedsAttention,
            ),
            (
                "claude-code",
                "working",
                "question",
                HerdrThreadStatus::Working,
            ),
            ("claude-code", "done", "", HerdrThreadStatus::Done),
            ("codex", "done", "question", HerdrThreadStatus::Done),
        ] {
            let terminal: DesktopTerminal = serde_json::from_value(serde_json::json!({
                "address": "t1",
                "cwd": "/repo",
                "created_ms": 0,
                "labels": { "zed.claude_triage": triage },
                "state": "live",
                "status": { "app": app, "state": state },
                "turn": 1,
            }))
            .unwrap();
            assert_eq!(terminal.thread_status(), Some(expected));
        }
    }

    #[test]
    fn herdr_thread_status_follows_unseen_turns() {
        let cases = [
            (Some("idle"), Some(0), "0", Some(HerdrThreadStatus::Idle)),
            (Some("idle"), Some(4), "3", Some(HerdrThreadStatus::Done)),
            (Some("done"), Some(4), "3", Some(HerdrThreadStatus::Done)),
            (Some("done"), Some(4), "4", Some(HerdrThreadStatus::Idle)),
            (Some("done"), None, "0", Some(HerdrThreadStatus::Done)),
            (
                Some("working"),
                Some(4),
                "3",
                Some(HerdrThreadStatus::Working),
            ),
            (
                Some("blocked"),
                Some(4),
                "4",
                Some(HerdrThreadStatus::NeedsAttention),
            ),
            (
                Some("error"),
                Some(4),
                "4",
                Some(HerdrThreadStatus::NeedsAttention),
            ),
            (Some("unknown"), Some(4), "3", None),
            (None, Some(4), "3", None),
        ];
        for (state, turn, seen, expected) in cases {
            let snapshot = serde_json::json!({
                "terminals": {
                    "t1": {
                        "address": "t1",
                        "cwd": "/repo",
                        "created_ms": 0,
                        "labels": { "herdr.space": "space", "herdr.seen": seen },
                        "state": "live",
                        "status": state.map(|state| serde_json::json!({ "state": state })),
                        "turn": turn,
                    }
                }
            });
            let sessions = desktop_sessions(snapshot.to_string().as_bytes()).unwrap();
            assert_eq!(
                sessions[&TerminalId::from_herdr_name("t1")].status,
                expected,
                "{state:?}, {turn:?}, seen {seen}"
            );
        }
    }
}
