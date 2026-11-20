> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# miZed

A code editor. miZed is a fork of [Zed](https://zed.dev) with my own changes on top: agent terminals that live in [Herdr Desktop](#herdr-desktop-terminals), a menu that starts a terminal agent with one key, and an inline prompt that a language server or a diff view can open under a line.

### [Download miZed](https://github.com/popoffvg/miZed/releases/latest)

This is a personal fork. I keep it because I want a few editor changes now, and most of them do not belong in the product scope of Zed. Zed is the real project. miZed follows its `main` branch and adds my changes on top.

**Platforms:** macOS (Apple Silicon). Build from source for other platforms.

**Stability:** I use it as my daily editor, and that is most of its QA. A sync with upstream can break things. Bug reports are welcome.

## Features

1. [Herdr Desktop terminals](#herdr-desktop-terminals): the Agent Panel and the Threads Sidebar show the terminals of Herdr Desktop
2. [Terminal agent menu](#terminal-agent-menu): start Claude Code, Codex, Pi, OpenCode, Cursor, Amp, Crush, Copilot, or Gemini with one key
3. [Inline input for language servers](#inline-input-for-language-servers): a language server asks for one line of text in a prompt under a line of the editor
4. [Line comments](#line-comments): comment on lines of a file, a diff, or a commit from the keyboard

### Herdr Desktop terminals

With this setting, each terminal thread of the Agent Panel runs in a terminal of Herdr Desktop:

```json
{
  "agent": {
    "threads_sidebar": {
      "herdr_sessions": true
    }
  }
}
```

- Put the `herdr-app` and `hc` commands of Herdr Desktop on your `PATH`.
- The Threads Sidebar shows the Herdr terminals under their projects, and hides the Zed agent threads.
- A new terminal thread opens a tab in the Herdr space of the project folder. If the folder has no space, miZed creates one.
- Close a row to remove it from the sidebar. The Herdr terminal continues to run.
- The terminal title comes from Herdr, and the sidebar reads it every three seconds.
- Drag with the left mouse button to select text in the terminal.

Herdr terminals are local. A remote project uses the usual Zed terminals.

### Terminal agent menu

When Herdr terminals are on, the **+** menu of the Agent Panel lists the terminal agents:

- **Terminal** starts a plain shell (`cmd-t`).
- Each agent on your `PATH` gets a number key (`1`, `2`, ...). An agent that is not installed shows "not installed".
- **New space** (`cmd-n`), **New worktree…**, and **Open worktree…** are in the same menu.

To start a different command for an entry, set it by the executable name:

```json
{
  "agent": {
    "terminal_agent_commands": {
      "claude": "claudex"
    }
  }
}
```

### Inline input for language servers

When miZed advertises `capabilities.experimental.zedInlineInput`, a language server can send a `zed/inlineInput` request. miZed then opens a one-line prompt under that line of the active editor:

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "method": "zed/inlineInput",
  "params": {
    "uri": "file:///project/main.rs",
    "line": 4,
    "placeholder": "Comment…",
    "value": ""
  }
}
```

Enter sends the text as a JSON string. Escape cancels, and the result is `null`. The protocol is in [docs/src/extensions/languages.md](docs/src/extensions/languages.md).

### Line comments

The `git: edit line comment` action (`git::EditLineComment`) opens the inline prompt under the selected lines of any file, diff view, or commit view. Enter saves the comment, and empty text deletes it. Each comment shows as a row under the last line it covers. A comment on a past commit ends with `read on <commit>`, because miZed keeps it on the line of the working tree.

miZed keeps the comments of a workspace in `~/Library/Application Support/Zed/line_comments/<workspace id>.json`, keyed by the absolute path of each file. A change to that file shows at once. Bind the action in your keymap, for example:

```json
[
  {
    "context": "Editor",
    "bindings": { "cmd-.": "git::EditLineComment" }
  }
]
```

## Install

### Manual download

From [Releases](https://github.com/popoffvg/miZed/releases), download the `.dmg`, open it, and drag the app to `/Applications`.

The build is not signed with an Apple certificate. Before the first start, remove the quarantine flag:

```sh
xattr -dr com.apple.quarantine "/Applications/miZed.app"
```

The app installs as **miZed**, with a purple icon, and runs next to the official Zed. It uses the `dev` release channel, so it does not download updates from zed.dev. It keeps its own workspace database (`0-dev`). It shares settings (`~/.config/zed`) and extensions (`~/Library/Application Support/Zed`) with Zed.

### Build from source

```sh
git clone git@github.com:popoffvg/miZed.git
cd miZed
CARGO_PROFILE_RELEASE_DEBUG=false script/bundle-mac -i    # release build without debug info, installs into /Applications
```

For the build requirements, see [Building Zed for macOS](./docs/src/development/macos.md).

## Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked. miZed uses the same licenses. Zed is developed by **Zed Industries, Inc.** This fork is not made or supported by Zed Industries.
