# abxglia-grok

A personal fork of [grok-build](https://github.com/xai-org/grok-build) with editor-based diff review and cross-build session sharing.

## Features

- **Binary isolation** — runs as `abxglia-grok` alongside the released `grok` without conflict (separate `GROK_HOME`)
- **Editor diff review** — when an edit hits the permission gate, opens a full-file diff in VS Code/Cursor with real file names
- **Auto-close tabs** — the diff tab closes automatically when you accept/reject the permission
- **Inline diff suppression** — terminal scrollback shows a one-line summary instead of the full diff (reviewed in the editor)
- **Cross-home session sharing** — resume sessions created by the released `grok` (one-way, copy-on-resume; originals never modified)

## Build

Requirements: Rust (pinned via `rust-toolchain.toml`), `protoc` (via `dotslash`), and Node.js for the extension.

```sh
# Use rustup's cargo (not system cargo) — it respects the pinned toolchain
PATH="$HOME/.cargo/bin:$PATH" cargo build -p xai-grok-pager-bin --release
```

The binary is at `target/release/abxglia-grok`.

## Install

### 1. Binary + isolation wrapper

```sh
# Install the binary
install -m755 target/release/abxglia-grok ~/.local/bin/abxglia-grok
```

Add this to `~/.bashrc` (or `~/.zshrc`) so `abxglia-grok` uses a separate data directory:

```bash
abxglia-grok() {
    GROK_HOME="$HOME/.abxglia-grok" command abxglia-grok "$@"
}
```

### 2. Configuration

Create `~/.abxglia-grok/config.toml`:

```toml
[ui]
diff_review_editor = "cursor"   # "code" | "cursor" | "auto" ($EDITOR) | unset (off)
permission_mode = "ask"         # "ask" | "auto" | "always-approve"
```

### 3. Cursor/VS Code extension

The extension provides the socket bridge (correct window targeting, real file names) and auto-close.

```sh
cd cursor-close-diff
npx @vscode/vsce package --allow-missing-repository
```

Install the resulting `.vsix`:
- **Cursor**: `cursor --install-extension abxglia-diff-bridge-*.vsix`
- **VS Code**: `code --install-extension abxglia-diff-bridge-*.vsix`

Or install manually by extracting the VSIX into `~/.cursor/extensions/abxglia.abxglia-diff-bridge-<version>/` and registering it in `extensions.json`.

Reload the editor window (`Ctrl+Shift+P` → "Reload Window") after installing. A status bar item `$(diff) grok: <project>` confirms the extension is active.

## Usage

```sh
cd /your/project
abxglia-grok
```

Ask the agent to edit a file. The diff opens in your editor; review it, then accept/reject in the terminal. The diff tab closes automatically.

To resume a session from the released `grok`:

```sh
abxglia-grok --resume <session-id>
# or use /resume in the TUI
```

The session is copied into `~/.abxglia-grok/sessions` before resuming; the original in `~/.grok` is never modified.

## Architecture

```
Grok (Rust)                          Extension (JS, in Cursor/VS Code)
──────────                           ──────────────────────────────────
On edit permission:                  On activate:
  1. Discover instances                1. Register instance descriptor
     (~/.abxglia-grok/instances/)         (workspace folder + socket path)
  2. Match cwd → pick window           2. Listen on Unix socket
  3. Send diff via socket              3. On request: open diff via
     {filePath, oldText, newText}          vscode.diff + TextDocumentContentProvider

On permission resolve:               On close command:
  Send {action: "close"} via socket    Close the active diff tab

CLI fallback:                         Close-signal watcher (legacy fallback)
  code/cursor --diff --reuse-window    (for when no socket instance exists)
```

## File layout

| Path | Contents |
|------|----------|
| `crates/.../diff_review.rs` | Diff review logic: socket bridge, instance discovery, CLI fallback |
| `crates/.../event_loop.rs` | Suspend arm that launches the editor |
| `crates/.../permissions.rs` | Permission hooks (arm diff, signal close) |
| `crates/.../paths.rs` | `foreign_sessions_root()` for cross-home sessions |
| `crates/.../persistence.rs` | Copy-on-resume for foreign sessions |
| `cursor-close-diff/` | The VS Code/Cursor extension |

## License

Apache-2.0 (same as upstream grok-build).
