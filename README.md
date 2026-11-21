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

Run `line comments: add line comment` to add a comment to the selected lines in a file, diff, or commit. Press Enter to save it. Click the gutter icon to collapse or expand the comment. Right-click the icon and select **Edit Line Comment** to edit it inline. Clear the text and press Enter to delete it. Run `line comments: list line comments` to search the active store and jump to a comment. Comments on past commits include the revision because miZed stores their lines against the working tree.

miZed keeps the comments of a workspace in `~/Library/Application Support/Zed/line_comments/<workspace id>.json`, keyed by the absolute path of each file. A change to that file shows at once. Bind the action in your keymap, for example:

```json
[
  {
    "context": "Editor",
    "bindings": { "cmd-.": "line_comments::AddLineComment" }
  }
]
```

Use `line comments: select line comment store` in a file or diff. Enter a name such as `review` or `notes`. miZed creates the store when you save its first comment. The selection applies to all editors in the workspace. Select `default` to see the existing comments again. Reopened workspaces start with the default store.

Build the comment reader from the miZed checkout with `cargo build --locked -p line_comments --bin list-line-comments`. Add the checkout's absolute `target/debug` path to your shell's `PATH`.

Run `hc get "$HC_TERMINAL_ID" --json` in a new miZed Herdr terminal. Find the `zed.workspace_id` label. Pass that ID to `list-line-comments <WORKSPACE_ID>`. Add `--store review` to read the named store. The command prints the store as JSON or `null` if it does not exist. If miZed uses a custom data directory, pass the same `--user-data-dir` to the reader.

### miZed MCP

Build miZed and the MCP bridge from the checkout. Start miZed with the address file set. Use the same address file in the Codex configuration.

```sh
cargo build --locked -p zed -p mized_mcp --bin zed --bin mized-mcp
MIZED_MCP_ADDRESS_FILE="$HOME/.mized-mcp-address" ./target/debug/zed "$PWD"
```

Add this entry to your Codex configuration. Replace the two paths with absolute paths. Start a new Codex session after you save the configuration.

```toml
[mcp_servers.mized]
command = "/path/to/zed/target/debug/mized-mcp"
args = ["/path/to/home/.mized-mcp-address"]
tool_timeout_sec = 900
```

- `add_comment`, `read_comment`, and `list_comments` use named stores per workspace. Pass `store` to select a named store. The default is `default`.
- `open_file` opens an existing local file by absolute path. Set `wait_for_close` to wait for the tab to close.
- `open_multibuffer` opens excerpts from local files in one editor tab. Give each excerpt an absolute path and an inclusive line range.
- `open_draft` opens supplied Markdown in an editor tab. Close the tab to return the edited text to the caller.

Each tool accepts `workspace_id`. Omit it to use the active local workspace. Use the workspace ID if miZed has multiple open workspaces and is out of focus.

#### Run MCP tools from a shell task

Use the same `mized-mcp` binary and address file in your tasks. The CLI prints JSON to stdout. It reports errors on stderr and exits with a nonzero status.

```sh
./target/debug/mized-mcp "$HOME/.mized-mcp-address" list
./target/debug/mized-mcp "$HOME/.mized-mcp-address" call list_comments '{"store":"review"}'
jq -n --rawfile text review.md '{title:"Review", text:$text}' |
  ./target/debug/mized-mcp "$HOME/.mized-mcp-address" call open_draft -
```

Use `list` to get the tool names and input schemas. Omit the JSON argument for a tool with no required inputs. Use `-` to pass a JSON object on stdin. The `open_draft` call waits until you close the tab.

#### Use MCP with the macOS release

Download the CLI archive from the same release as the app. Extract `mized-mcp` into a directory on your `PATH`. Quit miZed before you start it with the address file set.

```sh
MIZED_MCP_ADDRESS_FILE="$HOME/.mized-mcp-address" \
  /Applications/miZed.app/Contents/MacOS/zed "$PWD" &
mized-mcp "$HOME/.mized-mcp-address" list
```

Use the path to the extracted `mized-mcp` binary in your Codex configuration.

## Install

### Manual download

From [Releases](https://github.com/popoffvg/miZed/releases), download the `.dmg`, open it, and drag the app to `/Applications`.

The build is not signed with an Apple certificate. Before the first start, remove the quarantine flag:

```sh
xattr -dr com.apple.quarantine "/Applications/miZed.app"
```

The app installs as **miZed**, with an orange icon, and runs next to the official Zed. It uses the `dev` release channel, so it does not download updates from zed.dev. It keeps its own workspace database (`0-dev`). It shares settings (`~/.config/zed`) and extensions (`~/Library/Application Support/Zed`) with Zed.

### Build from source

```sh
git clone git@github.com:popoffvg/miZed.git
cd miZed
CARGO_PROFILE_RELEASE_DEBUG=false script/bundle-mac -i    # release build without debug info, installs into /Applications
```

For the build requirements, see [Building Zed for macOS](./docs/src/development/macos.md).

## Licensing

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked. miZed uses the same licenses. Zed is developed by **Zed Industries, Inc.** This fork is not made or supported by Zed Industries.
