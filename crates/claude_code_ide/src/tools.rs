//! The GPUI-backed [`Dispatcher`] implementation: this is where the wire
//! protocol meets Zed's editor state. Each `tools/call` is satisfied by reading
//! the active workspace, editor, and buffers and shaping the result into the
//! JSON the Claude Code CLI expects (mirroring the official extensions).

use crate::editor_context::{bounded_text, local_abs_path, utf16_range};
use crate::open_diff::{PendingDiffs, open_diff};
use crate::server::{Dispatcher, ProtocolError, ToolDescriptor, error_codes};
use editor::Editor;
use gpui::{AnyWindowHandle, App, AsyncApp, Entity, WeakEntity};
use language::{Buffer, DiagnosticSeverity};
use serde_json::{Value, json};
use std::{
    ops::Range,
    path::{Path, PathBuf},
};
use text::Point;
use workspace::{OpenOptions, Workspace};

/// Satisfies tool calls against a single Zed workspace.
///
/// Holds a weak handle to the workspace (so the server task never keeps the
/// window alive) plus a cheap clone of the async app context, which lets each
/// call hop onto the foreground thread to read entity state.
pub struct WorkspaceDispatcher {
    workspace: WeakEntity<Workspace>,
    window: Option<AnyWindowHandle>,
    cx: AsyncApp,
    /// The `openDiff` requests this connection is waiting on; see [`PendingDiffs`].
    pending: PendingDiffs,
}

impl WorkspaceDispatcher {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: Option<AnyWindowHandle>,
        cx: AsyncApp,
    ) -> Self {
        Self { workspace, window, cx, pending: PendingDiffs::default() }
    }

    fn get_current_selection(&self, cx: &mut AsyncApp) -> Result<Value, ProtocolError> {
        let payload = self
            .workspace
            .update(cx, |workspace, cx| {
                let Some(editor) = workspace.active_item_as::<Editor>(cx) else {
                    return json!({ "success": false, "message": "No active editor found" });
                };
                let mut payload = selection_payload(&editor, cx);
                payload["success"] = json!(true);
                payload
            })
            .map_err(|error| ProtocolError::internal(error.to_string()))?;

        Ok(mcp_text(payload))
    }

    fn get_workspace_folders(&self, cx: &mut AsyncApp) -> Result<Value, ProtocolError> {
        let paths = self
            .workspace
            .update(cx, |workspace, cx| {
                workspace
                    .project()
                    .read(cx)
                    .visible_worktrees(cx)
                    .map(|worktree| worktree.read(cx).abs_path().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
            .map_err(|error| ProtocolError::internal(error.to_string()))?;

        let folders = paths
            .iter()
            .map(|path| {
                json!({ "name": file_name(path), "uri": file_uri(path), "path": path })
            })
            .collect::<Vec<_>>();
        let root_path = paths.first().cloned().unwrap_or_default();

        Ok(mcp_text(json!({
            "success": true,
            "folders": folders,
            "rootPath": root_path,
        })))
    }

    /// Finds the open buffer for an absolute path, if any.
    ///
    /// Scans the open buffers rather than resolving a project path: that covers
    /// files `openFile` put into hidden worktrees, and it is the one place that
    /// compares paths the way Windows does (see [`same_path`]).
    fn buffer_for_path(&self, path: &str, cx: &mut AsyncApp) -> Option<Entity<Buffer>> {
        self.workspace
            .update(cx, |workspace, cx| {
                let buffers = open_buffers(workspace, cx);
                buffers.into_iter().find(|buffer| {
                    local_abs_path(buffer.read(cx), cx).is_some_and(|open| same_path(&open, path))
                })
            })
            .ok()
            .flatten()
    }

    fn get_open_editors(&self, cx: &mut AsyncApp) -> Result<Value, ProtocolError> {
        let tabs = self
            .workspace
            .update(cx, |workspace, cx| {
                let active = workspace.active_item_as::<Editor>(cx);
                workspace
                    .items_of_type::<Editor>(cx)
                    .filter_map(|editor| {
                        let is_active = active.as_ref() == Some(&editor);
                        let editor = editor.read(cx);
                        let buffer = editor.buffer().read(cx).as_singleton()?;
                        let buffer = buffer.read(cx);
                        let path = local_abs_path(buffer, cx)?;
                        let language = buffer
                            .language()
                            .map(|language| language.name().to_string())
                            .unwrap_or_else(|| "plaintext".to_owned());
                        let line_count = buffer.text_snapshot().max_point().row + 1;
                        let label = file_name(&path).to_owned();
                        Some(json!({
                            "uri": file_uri(&path),
                            "fileName": path,
                            "label": label,
                            "languageId": language,
                            "isActive": is_active,
                            "isDirty": buffer.is_dirty(),
                            "isPinned": false,
                            "isPreview": false,
                            "isUntitled": false,
                            "lineCount": line_count,
                            "groupIndex": 0,
                            "viewColumn": 1,
                            "isGroupActive": true,
                        }))
                    })
                    .collect::<Vec<_>>()
            })
            .map_err(|error| ProtocolError::internal(error.to_string()))?;
        Ok(mcp_text(json!({ "tabs": tabs })))
    }

    /// Answers in the shape the CLI parses: one text block holding a JSON array
    /// with one entry per file. The CLI reads only the first text block, expects
    /// 0-based positions with UTF-16 columns (as VS Code defines them), and
    /// drops any diagnostic whose severity is not one of the four names.
    fn get_diagnostics(&self, arguments: &Value, cx: &mut AsyncApp) -> Result<Value, ProtocolError> {
        let requested_uri = arguments.get("uri").and_then(Value::as_str).map(str::to_owned);
        let target = requested_uri.as_deref().map(path_from_uri);

        let files = self
            .workspace
            .update(cx, |workspace, cx| {
                let mut files = Vec::new();
                for buffer in open_buffers(workspace, cx) {
                    let buffer = buffer.read(cx);
                    let Some(path) = local_abs_path(buffer, cx) else {
                        continue;
                    };
                    if target.as_ref().is_some_and(|target| !same_path(target, &path)) {
                        continue;
                    }
                    let snapshot = buffer.snapshot();
                    let diagnostics = snapshot
                        .diagnostics_in_range::<Point, Point>(
                            Point::new(0, 0)..snapshot.max_point(),
                            false,
                        )
                        .map(|entry| {
                            let start = snapshot.point_to_point_utf16(entry.range.start);
                            let end = snapshot.point_to_point_utf16(entry.range.end);
                            json!({
                                "message": entry.diagnostic.message,
                                "severity": severity_name(entry.diagnostic.severity),
                                "range": {
                                    "start": { "line": start.row, "character": start.column },
                                    "end": { "line": end.row, "character": end.column },
                                },
                                "source": entry.diagnostic.source,
                            })
                        })
                        .collect::<Vec<_>>();
                    // The CLI compares this `uri` with the one it sent, without
                    // percent-decoding either, so a request for one file gets its
                    // own spelling back rather than our encoded form.
                    let uri = requested_uri.clone().unwrap_or_else(|| file_uri(&path));
                    files.push(json!({ "uri": uri, "diagnostics": diagnostics }));
                }
                files
            })
            .map_err(|error| ProtocolError::internal(error.to_string()))?;

        Ok(mcp_text(Value::Array(files)))
    }

    fn check_document_dirty(
        &self,
        arguments: &Value,
        cx: &mut AsyncApp,
    ) -> Result<Value, ProtocolError> {
        let path = required_string_field(arguments, "filePath")?;
        match self.buffer_for_path(&path, cx) {
            Some(buffer) => {
                let is_dirty = buffer.update(cx, |buffer, _| buffer.is_dirty());
                Ok(mcp_text(json!({
                    "success": true,
                    "filePath": path,
                    "isDirty": is_dirty,
                    "isUntitled": false,
                })))
            }
            None => Ok(mcp_text(
                json!({ "success": false, "message": format!("Document not open: {path}") }),
            )),
        }
    }

    async fn open_file(&self, arguments: Value, cx: &mut AsyncApp) -> Result<Value, ProtocolError> {
        let path = required_string_field(&arguments, "filePath")?;
        let start_line = arguments.get("startLine").and_then(Value::as_u64);
        let end_line = arguments.get("endLine").and_then(Value::as_u64);
        let window =
            self.window.ok_or_else(|| ProtocolError::internal("no window available"))?;
        let open_task = window
            .update(cx, |_root, window, cx| {
                self.workspace.upgrade().map(|workspace| {
                    workspace.update(cx, |workspace, cx| {
                        workspace.open_abs_path(
                            PathBuf::from(&path),
                            OpenOptions::default(),
                            window,
                            cx,
                        )
                    })
                })
            })
            .map_err(|error| ProtocolError::internal(error.to_string()))?
            .ok_or_else(|| ProtocolError::internal("workspace unavailable"))?;
        let item = open_task.await.map_err(|error| ProtocolError::internal(error.to_string()))?;

        // Optionally select the requested line range (1-indexed in the protocol).
        // Only an `Editor` can select; an image, say, opens as another item, and
        // the reply must not claim a selection that never happened.
        let selected = match (start_line, end_line) {
            (Some(start), Some(end)) => window
                .update(cx, |_root, window, cx| {
                    let Some(editor) = item.downcast::<Editor>() else {
                        return false;
                    };
                    editor.update(cx, |editor, cx| {
                        let start = Point::new(start.saturating_sub(1) as u32, 0);
                        let end = Point::new(end as u32, 0);
                        editor.change_selections(Default::default(), window, cx, |selections| {
                            selections.select_ranges([start..end]);
                        });
                    });
                    true
                })
                .unwrap_or(false),
            _ => false,
        };

        let message = match (start_line, end_line) {
            (Some(start), Some(end)) if selected => {
                format!("Opened file and selected lines {start} to {end}")
            }
            _ => format!("Opened file: {path}"),
        };
        Ok(json!({ "content": [{ "type": "text", "text": message }] }))
    }

    async fn save_document(
        &self,
        arguments: Value,
        cx: &mut AsyncApp,
    ) -> Result<Value, ProtocolError> {
        let path = required_string_field(&arguments, "filePath")?;
        let Some(buffer) = self.buffer_for_path(&path, cx) else {
            return Ok(mcp_text(
                json!({ "success": false, "message": format!("Document not open: {path}") }),
            ));
        };
        let save_task = self
            .workspace
            .update(cx, |workspace, cx| {
                workspace
                    .project()
                    .update(cx, |project, cx| project.save_buffer(buffer, cx))
            })
            .map_err(|error| ProtocolError::internal(error.to_string()))?;
        save_task.await.map_err(|error| ProtocolError::internal(error.to_string()))?;
        Ok(mcp_text(json!({ "success": true, "filePath": path })))
    }
}

/// Selected text beyond this many bytes is cut short, with a note saying so.
/// The CLI repeats the selection into the model's context with every prompt
/// while it stays active, so a select-all in a large file would spend a
/// sizeable share of even a 1M-token window on each turn. 32 KiB is roughly
/// 8k tokens, or 800 lines of code; anything larger is better mentioned as
/// `@file#Lstart-end`, which the CLI reads once.
const SELECTION_TEXT_LIMIT: usize = 32 * 1024;

/// The editor's file (empty for an untitled buffer) and its newest selection:
/// the raw material of both the selection payload and an `@` mention.
pub fn selection_range(editor: &Entity<Editor>, cx: &mut App) -> (String, Range<Point>) {
    editor.update(cx, |editor, cx| {
        let display_snapshot = editor.display_snapshot(cx);
        let cursor = editor.selections.newest::<Point>(&display_snapshot);
        let path = editor
            .buffer()
            .read(cx)
            .as_singleton()
            .and_then(|buffer| local_abs_path(buffer.read(cx), cx))
            .unwrap_or_default();
        (path, cursor.range())
    })
}

/// The editor's newest selection as the protocol describes one: the selected
/// text, the file's native path, and 0-based positions. This is both the
/// `getCurrentSelection` reply and the `selection_changed` notification.
pub fn selection_payload(editor: &Entity<Editor>, cx: &mut App) -> Value {
    let (path, range) = selection_range(editor, cx);
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);

    // Chunk by chunk, so a select-all in a huge file copies no more than the
    // cap on each cursor move rather than the whole buffer.
    let (mut text, truncated) =
        bounded_text(snapshot.text_for_range(range.clone()), SELECTION_TEXT_LIMIT);
    if truncated {
        text.push_str(
            "\n[selection cut short by Zed; mention the file and line range to read all of it]",
        );
    }
    let (start, end) = utf16_range(&snapshot, range.clone());

    json!({
        "text": text,
        "filePath": path,
        "fileUrl": file_uri(&path),
        "selection": {
            "start": { "line": start.row, "character": start.column },
            "end": { "line": end.row, "character": end.column },
            "isEmpty": range.start == range.end,
        }
    })
}

/// Formats an absolute path as a `file://` URI.
///
/// Windows needs more care than `format!("file://{path}")`: for `C:\dir\file`
/// that yields `file://C:\dir\file`, where `C:` parses as the URI *authority*
/// and backslashes are not separators, so the CLI cannot resolve it. Posix paths
/// already begin with `/` and so supply the third slash themselves.
fn file_uri(path: &str) -> String {
    // Only rewrite separators on Windows: elsewhere `\` is a legal character in
    // a file name, and replacing it would invent a directory boundary.
    let path = if cfg!(windows) { path.replace('\\', "/") } else { path.to_owned() };
    let path = percent_encode_path(&path);
    if path.starts_with('/') {
        format!("file://{path}")
    } else {
        format!("file:///{path}")
    }
}

/// Percent-encodes everything in a path that is not legal unescaped in a URI
/// path, per RFC 3986.
///
/// Without this a file called `what?.rs`, `a#b.rs` or `my notes.rs` produces a
/// URI the CLI parses as having a query, a fragment, or simply a different name
/// -- and since `getDiagnostics` filters by comparing the uri it is given
/// against the ones we emit, an unencoded path silently matches nothing.
///
/// `/` stays a separator, and `:` is left alone so a Windows drive reads as
/// `file:///C:/dir` rather than `file:///C%3A/dir`.
fn percent_encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Reverses [`percent_encode_path`]. A `%` that does not introduce two hex
/// digits is kept as written, so a path we did not encode survives unchanged.
fn percent_decode_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let decoded_byte = if bytes[index] == b'%' && index + 2 < bytes.len() {
            std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        } else {
            None
        };
        match decoded_byte {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Converts a `file://` URI back to a native absolute path; the inverse of
/// [`file_uri`]. Input that is not a URI is returned unchanged, because the CLI
/// sometimes sends a bare path where the schema asks for a uri.
fn path_from_uri(uri: &str) -> String {
    let Some(path) = uri.strip_prefix("file://") else {
        return uri.to_owned();
    };
    let path = percent_decode_path(path);
    let path = path.as_str();
    // `file:///C:/dir` strips to `/C:/dir`; drop the slash before a drive letter
    // so the result compares equal to the path Zed reports for the same file.
    let path = match path.strip_prefix('/') {
        Some(rest) if starts_with_drive_letter(rest) => rest,
        _ => path,
    };
    if cfg!(windows) { path.replace('/', "\\") } else { path.to_owned() }
}

/// Whether `path` starts with a Windows drive prefix such as `C:`.
fn starts_with_drive_letter(path: &str) -> bool {
    let mut chars = path.chars();
    matches!((chars.next(), chars.next()), (Some(drive), Some(':')) if drive.is_ascii_alphabetic())
}

/// The final component of `path`, for display. [`Path`] splits on the platform's
/// separators, so this handles `C:\dir\file` as well as `/dir/file`; splitting on
/// `'/'` alone never divides a Windows path and returns the whole thing.
fn file_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
}

fn required_string_field(arguments: &Value, field: &str) -> Result<String, ProtocolError> {
    arguments
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ProtocolError::new(error_codes::INVALID_REQUEST, format!("missing {field}")))
}

/// Maps Zed/LSP diagnostic severities to the names the CLI's validator accepts.
fn severity_name(severity: DiagnosticSeverity) -> &'static str {
    match severity {
        DiagnosticSeverity::ERROR => "Error",
        DiagnosticSeverity::WARNING => "Warning",
        DiagnosticSeverity::INFORMATION => "Info",
        DiagnosticSeverity::HINT => "Hint",
        // A language server reporting a severity outside 1-4 is out of spec;
        // the CLI drops anything it does not recognise, so answer with the
        // neutral name rather than lose the diagnostic.
        _ => "Info",
    }
}

/// Every buffer the project currently holds open, collected so the caller can
/// read each one without holding the store borrowed.
fn open_buffers(workspace: &Workspace, cx: &App) -> Vec<Entity<Buffer>> {
    workspace.project().read(cx).buffer_store().read(cx).buffers().collect()
}

/// Whether two absolute paths name the same file.
///
/// Windows paths compare case-insensitively and with either separator: the CLI
/// passes on whatever spelling the shell had (`c:\proj\SRC` after a `cd` typed
/// that way) while Zed reports the on-disk casing, and a bare path from the CLI
/// never went through [`path_from_uri`]'s separator rewrite. Elsewhere the
/// file system is case-sensitive and `\` is an ordinary character.
fn same_path(a: &str, b: &str) -> bool {
    let fold = |character: char| match character {
        '\\' => '/',
        other => other.to_ascii_lowercase(),
    };
    if cfg!(windows) { a.chars().map(fold).eq(b.chars().map(fold)) } else { a == b }
}

impl Dispatcher for WorkspaceDispatcher {
    fn tools(&self) -> Vec<ToolDescriptor> {
        let empty_object_schema = json!({
            "type": "object",
            "additionalProperties": false,
            "$schema": "http://json-schema.org/draft-07/schema#",
        });
        vec![
            ToolDescriptor {
                name: "getCurrentSelection",
                description: "Get the current text selection in the editor",
                input_schema: empty_object_schema.clone(),
            },
            ToolDescriptor {
                name: "getLatestSelection",
                description: "Get the most recent text selection (even if not in the active editor)",
                input_schema: empty_object_schema.clone(),
            },
            ToolDescriptor {
                name: "getWorkspaceFolders",
                description: "Get all workspace folders currently open in the IDE",
                input_schema: empty_object_schema.clone(),
            },
            ToolDescriptor {
                name: "getOpenEditors",
                description: "Get list of currently open files",
                input_schema: empty_object_schema.clone(),
            },
            ToolDescriptor {
                name: "closeAllDiffTabs",
                description: "Close all diff tabs in the editor",
                input_schema: empty_object_schema,
            },
            ToolDescriptor {
                name: "getDiagnostics",
                description: "Get language diagnostics (errors, warnings) from the editor",
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "properties": {
                        "uri": { "type": "string" },
                    },
                }),
            },
            ToolDescriptor {
                name: "checkDocumentDirty",
                description: "Check if a document has unsaved changes (is dirty)",
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "properties": { "filePath": { "type": "string" } },
                    "required": ["filePath"],
                }),
            },
            ToolDescriptor {
                name: "saveDocument",
                description: "Save a document with unsaved changes",
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "properties": { "filePath": { "type": "string" } },
                    "required": ["filePath"],
                }),
            },
            ToolDescriptor {
                name: "openFile",
                description: "Open a file in the editor and optionally select a range of text",
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "properties": {
                        "filePath": { "type": "string" },
                        "preview": { "type": "boolean" },
                        "startLine": { "type": "integer" },
                        "endLine": { "type": "integer" },
                        "startText": { "type": "string" },
                        "endText": { "type": "string" },
                        "makeFrontmost": { "type": "boolean" },
                    },
                    "required": ["filePath"],
                }),
            },
            ToolDescriptor {
                name: "openDiff",
                description: "Open a diff view comparing old file content with new file content",
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "properties": {
                        "old_file_path": { "type": "string" },
                        "new_file_path": { "type": "string" },
                        "new_file_contents": { "type": "string" },
                        "tab_name": { "type": "string" },
                    },
                    "required": ["old_file_path", "new_file_path", "new_file_contents", "tab_name"],
                }),
            },
        ]
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, ProtocolError> {
        // Reading entity state has to happen on the foreground thread; a cloned
        // `AsyncApp` lets `WeakEntity::update` marshal us there.
        let mut cx = self.cx.clone();
        match name {
            "getCurrentSelection" => self.get_current_selection(&mut cx),
            "getLatestSelection" => self.get_current_selection(&mut cx),
            "getWorkspaceFolders" => self.get_workspace_folders(&mut cx),
            "getOpenEditors" => self.get_open_editors(&mut cx),
            "getDiagnostics" => self.get_diagnostics(&arguments, &mut cx),
            "checkDocumentDirty" => self.check_document_dirty(&arguments, &mut cx),
            "openFile" => self.open_file(arguments, &mut cx).await,
            "saveDocument" => self.save_document(arguments, &mut cx).await,
            // The CLI sends `close_tab` when the user aborts (Esc) and on exit,
            // having already discarded that request's answer. Removing the entry
            // rejects it (see `PendingDiffs`), which closes its tab and toast.
            "close_tab" => {
                let tab_name = required_string_field(&arguments, "tab_name")?;
                self.pending.borrow_mut().remove(&tab_name);
                Ok(mcp_text(json!({ "success": true })))
            }
            "closeAllDiffTabs" => {
                let closed = self.pending.borrow_mut().drain().count();
                Ok(mcp_text(json!({ "closedCount": closed })))
            }
            "openDiff" => {
                open_diff(self.workspace.clone(), self.window, &self.pending, arguments, &mut cx)
                    .await
            }
            other => Err(ProtocolError::method_not_found(other)),
        }
    }
}

/// Wraps a tool's payload in the MCP result envelope: the payload is
/// JSON-stringified into a single text content block, exactly as the official
/// extensions do.
fn mcp_text(payload: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": payload.to_string() }] })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path shape this platform actually produces; every conversion below is
    /// checked against what Zed reports for a real file.
    fn native_path() -> &'static str {
        if cfg!(windows) { r"C:\Users\user\project" } else { "/home/user/project" }
    }

    #[test]
    fn file_uri_keeps_the_posix_form() {
        // An absolute posix path supplies the third slash itself, so this is the
        // behaviour the crate already had everywhere except Windows.
        assert_eq!(file_uri("/home/user/project"), "file:///home/user/project");
    }

    #[cfg(windows)]
    #[test]
    fn file_uri_rewrites_windows_paths() {
        // Three slashes (empty authority) and forward separators, or the CLI
        // reads `C:` as the host and cannot resolve the file.
        assert_eq!(file_uri(r"C:\Users\user\project"), "file:///C:/Users/user/project");
    }

    #[cfg(not(windows))]
    #[test]
    fn file_uri_escapes_backslashes_off_windows() {
        // `\` is an ordinary character in a posix file name, so it must not
        // become a directory boundary -- but it is not legal unescaped in a URI
        // either. Percent-encoding satisfies both: no boundary is invented, and
        // the name comes back intact.
        let path = r"/home/user/we\ird.txt";
        assert_eq!(file_uri(path), "file:///home/user/we%5Cird.txt");
        assert_eq!(path_from_uri(&file_uri(path)), path);
    }

    #[test]
    fn file_uri_escapes_what_would_otherwise_change_the_uri() {
        // Unescaped, `?` opens a query and `#` a fragment, so the CLI would
        // resolve a shorter path than the one we named; a space is simply not
        // legal. `:` stays readable so a drive letter reads as `C:`.
        assert_eq!(file_uri("/home/user/what?.rs"), "file:///home/user/what%3F.rs");
        assert_eq!(file_uri("/home/user/a#b.rs"), "file:///home/user/a%23b.rs");
        assert_eq!(file_uri("/home/user/my notes.rs"), "file:///home/user/my%20notes.rs");
        assert_eq!(file_uri("/home/user/plain.rs"), "file:///home/user/plain.rs");
    }

    #[cfg(not(windows))]
    #[test]
    fn path_from_uri_round_trips_awkward_names() {
        for path in [
            "/home/user/my notes.rs",
            "/home/user/a#b.rs",
            "/home/user/what?.rs",
            "/home/user/100% done.rs",
            "/home/user/\u{fc}n\u{ef}c\u{f8}de.rs",
        ] {
            assert_eq!(path_from_uri(&file_uri(path)), path, "round trip of {path}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn path_from_uri_leaves_an_incomplete_escape_alone() {
        // `%zz` is not an escape, and a path we never encoded still has to
        // survive -- the CLI sometimes sends one.
        assert_eq!(path_from_uri("file:///home/user/100%zz.rs"), "/home/user/100%zz.rs");
        assert_eq!(path_from_uri("file:///home/user/trailing%"), "/home/user/trailing%");
    }

    #[test]
    fn path_from_uri_inverts_file_uri() {
        let path = native_path();
        assert_eq!(path_from_uri(&file_uri(path)), path);
    }

    #[test]
    fn path_from_uri_passes_plain_paths_through() {
        // The CLI sometimes sends a bare path where the schema says uri.
        let path = native_path();
        assert_eq!(path_from_uri(path), path);
    }

    #[test]
    fn file_name_takes_the_last_component() {
        assert_eq!(file_name("/home/user/project"), "project");
        assert_eq!(file_name(native_path()), "project");
        // A bare name, and a root with no component, both fall back to the input.
        assert_eq!(file_name("project"), "project");
        assert_eq!(file_name("/"), "/");
    }

    #[cfg(windows)]
    #[test]
    fn same_path_folds_case_and_separators_on_windows() {
        assert!(same_path(r"c:\dev\proj\SRC\main.rs", r"C:\dev\proj\src\main.rs"));
        assert!(same_path("C:/dev/proj/src/main.rs", r"C:\dev\proj\src\main.rs"));
        assert!(!same_path(r"C:\dev\proj\src\main.rs", r"C:\dev\proj\src\main.rss"));
    }

    #[cfg(not(windows))]
    #[test]
    fn same_path_is_exact_off_windows() {
        assert!(same_path("/home/user/Main.rs", "/home/user/Main.rs"));
        assert!(!same_path("/home/user/Main.rs", "/home/user/main.rs"));
        assert!(!same_path(r"/home/user/we\ird.txt", "/home/user/we/ird.txt"));
    }

    #[test]
    fn starts_with_drive_letter_only_matches_a_drive() {
        assert!(starts_with_drive_letter("C:/Users"));
        assert!(starts_with_drive_letter("z:"));
        assert!(!starts_with_drive_letter("/home"));
        assert!(!starts_with_drive_letter("1:/nope"));
    }
}
