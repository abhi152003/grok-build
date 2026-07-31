//! Open a visual diff in the user's editor when an edit hits the permission
//! gate (`[ui] diff_review_editor`).

use std::sync::OnceLock;

use serde_json::Value;
use tempfile::NamedTempFile;
use wait_timeout::ChildExt;
use tracing::warn;

/// Whether editor diff review is enabled. Cached once per process.
pub fn editor_review_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let Ok(config) = xai_grok_shell::config::load_effective_config() else {
            return false;
        };
        config
            .get("ui")
            .and_then(|ui| ui.get("diff_review_editor"))
            .and_then(|v| v.as_str())
            .is_some_and(|s| ReviewMode::from_setting(s).is_some())
    })
}

/// Signal the editor extension to close the active diff tab. Called when a
/// permission resolves. Sends `{action: "close"}` via the socket bridge for
/// instant, race-free closing; falls back to the file-based signal if no
/// socket instance is available.
pub fn signal_close_diff_tab() {
    if !editor_review_enabled() {
        return;
    }
    let instances = discover_instances();
    let socket_ok = instances
        .iter()
        .any(|inst| send_close_via_socket(&inst.socket_path));
    if !socket_ok {
        write_close_signal_file();
    }
}

fn send_close_via_socket(socket_path: &std::path::Path) -> bool {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let request = serde_json::json!({ "action": "close" });
    let Ok(request_bytes) = serde_json::to_vec(&request) else {
        return false;
    };

    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));

    let _ = stream.write_all(&request_bytes);
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        return true;
    }
    serde_json::from_str::<Value>(&response)
        .ok()
        .and_then(|v| v.get("ok").and_then(Value::as_bool))
        .unwrap_or(true)
}

/// Fallback close signal via file watcher (used when no socket instance exists).
fn write_close_signal_file() {
    let Some(home) = xai_grok_config::user_grok_home() else {
        return;
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::File::create(home.join(".close-diff-signal")) {
        let _ = write!(f, "{timestamp}");
    }
}

// ─── Socket bridge ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct EditorInstance {
    socket_path: std::path::PathBuf,
    workspace_folder: String,
}

/// Read registered editor instances from `~/.abxglia-grok/instances/`.
fn discover_instances() -> Vec<EditorInstance> {
    use std::io::Read as _;

    let Some(home) = xai_grok_config::user_grok_home() else {
        return vec![];
    };
    let Ok(entries) = std::fs::read_dir(home.join("instances")) else {
        return vec![];
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                return None;
            }
            let mut contents = String::new();
            std::fs::File::open(&path)
                .and_then(|mut f| f.read_to_string(&mut contents))
                .ok()?;
            let json = serde_json::from_str::<Value>(&contents).ok()?;
            let socket_str = json.get("socketPath").and_then(Value::as_str)?;
            let socket_path = std::path::PathBuf::from(socket_str);
            if !socket_path.exists() {
                return None;
            }
            Some(EditorInstance {
                socket_path,
                workspace_folder: json
                    .get("workspaceFolder")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

/// Find the editor instance whose workspace best matches `cwd`.
fn find_instance_for_cwd(cwd: &std::path::Path) -> Option<EditorInstance> {
    let cwd_str = cwd.to_string_lossy();
    discover_instances()
        .into_iter()
        .filter(|inst| {
            !inst.workspace_folder.is_empty()
                && (cwd_str.starts_with(&inst.workspace_folder)
                    || inst.workspace_folder.starts_with(cwd_str.as_ref()))
        })
        .max_by_key(|inst| inst.workspace_folder.len())
}

/// Send a diff request to an editor instance's socket.
fn send_diff_via_socket(
    socket_path: &std::path::Path,
    file_path: &str,
    old_text: &str,
    new_text: &str,
) -> bool {
    use std::io::{Read as _, Write as _};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let request = serde_json::json!({
        "filePath": file_path,
        "oldText": old_text,
        "newText": new_text,
    });
    let Ok(request_bytes) = serde_json::to_vec(&request) else {
        return false;
    };

    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "diff_review: failed to connect to editor socket");
            return false;
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));

    let _ = stream.write_all(&request_bytes);
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        return true;
    }
    serde_json::from_str::<Value>(&response)
        .ok()
        .and_then(|v| v.get("ok").and_then(Value::as_bool))
        .unwrap_or(true)
}

// ─── Review mode ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewMode {
    Code,
    Cursor,
    Auto,
}

impl ReviewMode {
    pub fn from_setting(setting: &str) -> Option<Self> {
        match setting.trim().to_ascii_lowercase().as_str() {
            "code" => Some(Self::Code),
            "cursor" => Some(Self::Cursor),
            "auto" => Some(Self::Auto),
            "" | "off" | "none" => None,
            _ => None,
        }
    }

    pub fn is_gui(&self) -> bool {
        matches!(self, Self::Code | Self::Cursor)
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Code => "VS Code",
            Self::Cursor => "Cursor",
            Self::Auto => "editor",
        }
    }
}

// ─── Pending diff review ─────────────────────────────────────────────────

pub struct PendingEditorDiff {
    mode: ReviewMode,
    old_tmp: NamedTempFile,
    new_tmp: NamedTempFile,
    file_path: String,
    cwd: std::path::PathBuf,
}

impl PendingEditorDiff {
    /// Build from a permission request's `raw_input`. Returns `None` for
    /// unsupported edit kinds (HashlineEdit/ApplyPatch).
    pub fn try_build(raw_input: &Value, mode: ReviewMode, cwd: std::path::PathBuf) -> Option<Self> {
        let variant = raw_input.get("variant").and_then(Value::as_str)?;
        let file_path = raw_input
            .get("file_path")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        let (old_text, new_text): (String, String) = match variant {
            "SearchReplace" => {
                let old_fragment = raw_input
                    .get("old_string")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let new_fragment = raw_input
                    .get("new_string")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let replace_all = raw_input
                    .get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let full_old = std::fs::read_to_string(&file_path).unwrap_or_default();
                let full_new = if replace_all {
                    full_old.replace(old_fragment, new_fragment)
                } else {
                    full_old.replacen(old_fragment, new_fragment, 1)
                };
                (full_old, full_new)
            }
            "Write" => {
                let new = raw_input
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let old = std::fs::read_to_string(&file_path).unwrap_or_default();
                (old, new)
            }
            _ => return None,
        };

        let old_tmp = write_temp(&old_text)?;
        let new_tmp = write_temp(&new_text)?;
        Some(Self {
            mode,
            old_tmp,
            new_tmp,
            file_path,
            cwd,
        })
    }

    /// Launch the editor. For GUI editors, tries the socket bridge first
    /// (correct window + real file names), falling back to the CLI. Returns
    /// `true` if the diff opened successfully.
    pub fn run(&self) -> bool {
        let old_path = self.old_tmp.path();
        let new_path = self.new_tmp.path();
        match self.mode {
            ReviewMode::Code | ReviewMode::Cursor => {
                if let Some(instance) = find_instance_for_cwd(&self.cwd) {
                    let old_text = std::fs::read_to_string(old_path).unwrap_or_default();
                    let new_text = std::fs::read_to_string(new_path).unwrap_or_default();
                    if send_diff_via_socket(
                        &instance.socket_path,
                        &self.file_path,
                        &old_text,
                        &new_text,
                    ) {
                        return true;
                    }
                }
                let bin = match self.mode {
                    ReviewMode::Code => "code",
                    ReviewMode::Cursor => "cursor",
                    _ => unreachable!(),
                };
                spawn_gui_diff(bin, old_path, new_path)
            }
            ReviewMode::Auto => {
                let editor = std::env::var("VISUAL")
                    .or_else(|_| std::env::var("EDITOR"))
                    .unwrap_or_else(|_| "vi".to_string());
                if let Err(e) = std::process::Command::new(&editor)
                    .arg(old_path)
                    .arg(new_path)
                    .status()
                {
                    warn!(error = %e, editor = %editor, "diff_review: terminal editor failed");
                }
                true
            }
        }
    }

    pub fn is_gui(&self) -> bool {
        self.mode.is_gui()
    }

    pub fn display_name(&self) -> &'static str {
        self.mode.display_name()
    }

    /// Leak temp files so they survive after drop (GUI editors read async).
    pub fn leak_temp_files(self) {
        let _ = self.old_tmp.keep();
        let _ = self.new_tmp.keep();
    }
}

/// Spawn a GUI diff viewer detached. Returns `true` if it opened successfully.
/// Probes for 3s after spawn to detect failed launches.
fn spawn_gui_diff(bin: &str, old_path: &std::path::Path, new_path: &std::path::Path) -> bool {
    let mut command = std::process::Command::new(bin);
    command
        .arg("--diff")
        .arg("--reuse-window")
        .arg(old_path)
        .arg(new_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    xai_tty_utils::detach_std_command(&mut command);
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, bin, "diff_review: failed to spawn GUI diff viewer");
            return false;
        }
    };
    match child.wait_timeout(std::time::Duration::from_secs(3)) {
        Ok(Some(status)) if status.success() => true,
        Ok(Some(status)) => {
            warn!(bin, code = ?status.code(), "diff_review: GUI diff viewer exited with error");
            false
        }
        Ok(None) => true,
        Err(e) => {
            warn!(error = %e, bin, "diff_review: failed to wait on GUI diff viewer");
            false
        }
    }
}

fn write_temp(text: &str) -> Option<NamedTempFile> {
    match NamedTempFile::new() {
        Ok(mut f) => {
            use std::io::Write as _;
            if let Err(e) = f.write_all(text.as_bytes()) {
                warn!(error = %e, "diff_review: failed to write temp file");
                return None;
            }
            if let Err(e) = f.flush() {
                warn!(error = %e, "diff_review: failed to flush temp file");
                return None;
            }
            Some(f)
        }
        Err(e) => {
            warn!(error = %e, "diff_review: failed to create temp file");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_mode_parses_known_values() {
        assert_eq!(ReviewMode::from_setting("code"), Some(ReviewMode::Code));
        assert_eq!(ReviewMode::from_setting("Cursor"), Some(ReviewMode::Cursor));
        assert_eq!(ReviewMode::from_setting("auto"), Some(ReviewMode::Auto));
        assert_eq!(ReviewMode::from_setting("off"), None);
        assert_eq!(ReviewMode::from_setting(""), None);
        assert_eq!(ReviewMode::from_setting("bogus"), None);
    }

    #[test]
    fn gui_vs_terminal_classification() {
        assert!(ReviewMode::Code.is_gui());
        assert!(ReviewMode::Cursor.is_gui());
        assert!(!ReviewMode::Auto.is_gui());
    }

    #[test]
    fn try_build_search_replace() {
        let raw = serde_json::json!({
            "variant": "SearchReplace",
            "file_path": "/tmp/x.rs",
            "old_string": "let x = 1;",
            "new_string": "let x = 2;",
        });
        let d = PendingEditorDiff::try_build(&raw, ReviewMode::Code, std::path::PathBuf::from("/tmp"))
            .expect("SearchReplace builds");
        let old = std::fs::read_to_string(d.old_tmp.path()).unwrap();
        let new = std::fs::read_to_string(d.new_tmp.path()).unwrap();
        assert_eq!(old, "let x = 1;");
        assert_eq!(new, "let x = 2;");
    }

    #[test]
    fn try_build_unsupported_variant_returns_none() {
        let raw = serde_json::json!({ "variant": "HashlineEdit", "file_path": "/tmp/x.rs" });
        assert!(PendingEditorDiff::try_build(&raw, ReviewMode::Code, std::path::PathBuf::from("/tmp")).is_none());
    }

    #[test]
    fn try_build_missing_variant_returns_none() {
        let raw = serde_json::json!({ "file_path": "/tmp/x.rs" });
        assert!(PendingEditorDiff::try_build(&raw, ReviewMode::Code, std::path::PathBuf::from("/tmp")).is_none());
    }
}
