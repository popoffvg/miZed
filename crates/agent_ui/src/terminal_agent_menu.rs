use crate::{AgentPanel, AgentThreadSource};
use agent_settings::AgentSettings;
use gpui::{Action as _, App, Entity, Focusable as _, Keystroke, WeakEntity, Window};
use settings::Settings;
use ui::{ContextMenu, ContextMenuEntry, IconName};

const TERMINAL_AGENTS: &[(&str, &str, IconName)] = &[
    ("Claude Code", "claude", IconName::AiClaude),
    ("Codex", "codex --no-daemon", IconName::AiOpenAi),
    ("Pi", "pi", IconName::TerminalAlt),
    ("OpenCode", "opencode", IconName::AiOpenCode),
    ("Cursor", "cursor-agent", IconName::CursorIBeam),
    ("Amp", "amp", IconName::BoltFilled),
    ("Crush", "crush", IconName::Terminal),
    ("Copilot", "copilot", IconName::Copilot),
    ("Gemini", "gemini", IconName::AiGemini),
];

fn launch_entry(
    panel: WeakEntity<AgentPanel>,
    label: String,
    icon: IconName,
    command: Option<String>,
) -> ContextMenuEntry {
    ContextMenuEntry::new(label)
        .icon(icon)
        .handler(move |window, cx| {
            panel
                .update(cx, |panel, cx| {
                    panel.new_terminal_with_command(
                        None,
                        command.clone(),
                        AgentThreadSource::AgentPanel,
                        window,
                        cx,
                    )
                })
                .ok();
        })
}

pub(super) fn build(
    panel: WeakEntity<AgentPanel>,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |menu, _, cx| {
        let menu = match panel.upgrade() {
            Some(panel) => menu.context(panel.read(cx).focus_handle(cx)),
            None => return menu,
        };
        let mut terminal = launch_entry(panel.clone(), "Terminal".into(), IconName::Terminal, None);
        if let Ok(key) = Keystroke::parse(if cfg!(target_os = "macos") {
            "cmd-t"
        } else {
            "ctrl-t"
        }) {
            terminal = terminal.shortcut(key);
        }
        let mut menu = menu.item(terminal).header("Start an agent");
        let settings = AgentSettings::get_global(cx);
        let mut agents = TERMINAL_AGENTS
            .iter()
            .map(|&(label, default_command, icon)| {
                let executable = default_command
                    .split_once(' ')
                    .map_or(default_command, |(program, _)| program);
                let command = settings.terminal_agent_commands.get(executable);
                let available = command.map_or_else(
                    || which::which(executable).is_ok(),
                    |value| !value.trim().is_empty(),
                );
                (
                    label,
                    icon,
                    command.map(String::as_str).unwrap_or(default_command),
                    available,
                )
            })
            .collect::<Vec<_>>();
        agents.sort_by_key(|(_, _, _, available)| !available);
        for (index, (label, icon, command, available)) in agents.into_iter().enumerate() {
            let label = if available {
                label.to_owned()
            } else {
                format!("{label}    not installed")
            };
            let mut entry = launch_entry(panel.clone(), label, icon, Some(command.to_owned()))
                .disabled(!available);
            if available {
                entry = entry.shortcut(Keystroke {
                    modifiers: Default::default(),
                    key: (index + 1).to_string(),
                    key_char: None,
                });
            }
            menu = menu.item(entry);
        }
        let new_space = workspace::Open {
            create_new_window: Some(false),
        };
        let mut space = ContextMenuEntry::new("New space")
            .icon(IconName::Plus)
            .handler(move |window, cx| window.dispatch_action(new_space.boxed_clone(), cx));
        if let Ok(key) = Keystroke::parse(if cfg!(target_os = "macos") {
            "cmd-n"
        } else {
            "ctrl-n"
        }) {
            space = space.shortcut(key);
        }
        menu.separator()
            .item(space)
            .action(
                "New worktree…",
                Box::new(zed_actions::CreateWorktree {
                    worktree_name: None,
                    branch_target: Default::default(),
                }),
            )
            .action("Open worktree…", Box::new(zed_actions::git::Worktree))
    })
}
