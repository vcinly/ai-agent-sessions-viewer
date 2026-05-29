use super::ProviderInfo;
use crate::models::{ClaudeMessage, ClaudeProject, ClaudeSession, TokenUsage};
use crate::utils::{
    build_provider_message, is_safe_storage_id, search_json_value_case_insensitive,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const PROVIDER_ID: &str = "alma";
const DISPLAY_NAME: &str = "Alma";
const DB_FILE: &str = "chat_threads.db";
const TEMP_PROJECT_ID: &str = "temporary";

#[derive(Debug, Clone)]
struct ThreadRow {
    id: String,
    title: String,
    workspace_id: Option<String>,
    workspace_path: Option<String>,
    workspace_name: Option<String>,
    is_temporary: bool,
    created_at: String,
    updated_at: String,
    first_message_time: Option<String>,
    last_message_time: Option<String>,
    message_count: usize,
    has_tool_use: bool,
    has_errors: bool,
}

#[derive(Debug, Clone)]
struct UsageRow {
    input: Option<u32>,
    output: Option<u32>,
    cache_read: Option<u32>,
    cache_write: Option<u32>,
}

/// Detect an Alma installation.
pub fn detect() -> Option<ProviderInfo> {
    let base_path = get_base_path()?;
    let db_path = Path::new(&base_path).join(DB_FILE);

    Some(ProviderInfo {
        id: PROVIDER_ID.to_string(),
        display_name: DISPLAY_NAME.to_string(),
        base_path,
        is_available: db_path.is_file(),
    })
}

/// Resolve the Alma data directory.
///
/// Lookup precedence:
/// 1. `$ALMA_HOME`
/// 2. Windows `%APPDATA%\alma`
/// 3. macOS `~/Library/Application Support/alma`
/// 4. `~/.alma`
pub fn get_base_path() -> Option<String> {
    let candidates = base_path_candidates();
    candidates
        .iter()
        .find(|path| path.join(DB_FILE).is_file())
        .or_else(|| candidates.iter().find(|path| path.exists()))
        .map(|path| path.to_string_lossy().to_string())
}

fn base_path_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Ok(home) = std::env::var("ALMA_HOME") {
        paths.push(PathBuf::from(home));
    }

    if let Ok(appdata) = std::env::var("APPDATA") {
        paths.push(PathBuf::from(appdata).join("alma"));
    }

    if let Some(home) = dirs::home_dir() {
        paths.extend(home_base_path_candidates(&home));
    }

    paths
}

fn home_base_path_candidates(home: &Path) -> [PathBuf; 2] {
    [
        home.join("Library")
            .join("Application Support")
            .join("alma"),
        home.join(".alma"),
    ]
}

/// Scan Alma workspaces as projects.
pub fn scan_projects() -> Result<Vec<ClaudeProject>, String> {
    let base_path = get_base_path().ok_or_else(|| "Alma base path not found".to_string())?;
    scan_projects_from_path(&base_path)
}

/// Scan Alma workspaces as projects from an explicit base path.
pub fn scan_projects_from_path(base_path: &str) -> Result<Vec<ClaudeProject>, String> {
    crate::utils::require_absolute_path(base_path, "Alma base path")?;
    let conn = open_db(base_path).ok_or_else(|| "Alma database not found".to_string())?;
    let rows = load_thread_rows(&conn, None)?;

    let mut projects = std::collections::BTreeMap::<String, ClaudeProject>::new();

    for row in rows {
        let (project_id, project_name, actual_path) = project_identity(&row);
        let entry = projects
            .entry(project_id.clone())
            .or_insert_with(|| ClaudeProject {
                name: project_name,
                path: format!("alma://workspace/{project_id}"),
                actual_path,
                session_count: 0,
                message_count: 0,
                last_modified: row.updated_at.clone(),
                git_info: None,
                provider: Some(PROVIDER_ID.to_string()),
                storage_type: Some("sqlite".to_string()),
                custom_directory_label: None,
            });

        entry.session_count += 1;
        entry.message_count += row.message_count;
        if row.updated_at > entry.last_modified {
            entry.last_modified = row.updated_at;
        }
    }

    let mut result: Vec<ClaudeProject> = projects.into_values().collect();
    result.retain(|project| project.session_count > 0);
    result.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    Ok(result)
}

/// Load Alma thread sessions for a workspace project.
pub fn load_sessions(
    project_path: &str,
    _exclude_sidechain: bool,
) -> Result<Vec<ClaudeSession>, String> {
    let base_path = get_base_path().ok_or_else(|| "Alma not found".to_string())?;
    let project_id = parse_workspace_project_path(project_path)
        .ok_or_else(|| format!("Invalid Alma project path: {project_path}"))?;
    let conn = open_db(&base_path).ok_or_else(|| "Alma database not found".to_string())?;
    let rows = load_thread_rows(&conn, Some(project_id))?;

    let mut sessions: Vec<ClaudeSession> = rows
        .into_iter()
        .map(|row| {
            let (project_id, project_name, _) = project_identity(&row);
            let first = row
                .first_message_time
                .clone()
                .unwrap_or_else(|| row.created_at.clone());
            let last = row
                .last_message_time
                .clone()
                .unwrap_or_else(|| row.updated_at.clone());

            ClaudeSession {
                session_id: format!("alma://thread/{}", row.id),
                actual_session_id: row.id.clone(),
                file_path: format!("alma://thread/{}", row.id),
                project_name,
                message_count: row.message_count,
                first_message_time: first,
                last_message_time: last,
                last_modified: row.updated_at,
                has_tool_use: row.has_tool_use,
                has_errors: row.has_errors,
                summary: non_empty_string(row.title),
                is_renamed: false,
                provider: Some(PROVIDER_ID.to_string()),
                storage_type: Some("sqlite".to_string()),
                entrypoint: if row.is_temporary {
                    Some("temporary".to_string())
                } else {
                    Some(project_id)
                },
            }
        })
        .collect();

    sessions.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    Ok(sessions)
}

/// Load normalized messages for a single Alma thread.
pub fn load_messages(session_path: &str) -> Result<Vec<ClaudeMessage>, String> {
    let thread_id = parse_thread_session_path(session_path)
        .ok_or_else(|| format!("Invalid Alma session path: {session_path}"))?;
    let base_path = get_base_path().ok_or_else(|| "Alma not found".to_string())?;
    let conn = open_db(&base_path).ok_or_else(|| "Alma database not found".to_string())?;
    load_messages_with_conn(&conn, thread_id)
}

/// Search Alma messages.
pub fn search(query: &str, limit: usize) -> Result<Vec<ClaudeMessage>, String> {
    let base_path = get_base_path().ok_or_else(|| "Alma not found".to_string())?;
    let conn = open_db(&base_path).ok_or_else(|| "Alma database not found".to_string())?;
    let query_lower = query.to_lowercase();
    let escaped = escape_like_pattern(&query_lower);
    let search_pattern = format!("%{escaped}%");

    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT thread_id
             FROM chat_messages
             WHERE LOWER(message) LIKE ?1 ESCAPE '\\'
             ORDER BY timestamp DESC",
        )
        .map_err(|e| e.to_string())?;

    let thread_ids: Vec<String> = stmt
        .query_map(rusqlite::params![search_pattern], |row| row.get(0))
        .map_err(|e| e.to_string())?
        .filter_map(std::result::Result::ok)
        .collect();

    let mut results = Vec::new();
    for thread_id in thread_ids {
        let messages = load_messages_with_conn(&conn, &thread_id)?;
        for message in messages {
            if results.len() >= limit {
                return Ok(results);
            }
            if let Some(content) = &message.content {
                if search_json_value_case_insensitive(content, &query_lower) {
                    results.push(message);
                }
            }
        }
    }

    Ok(results)
}

fn open_db(base_path: &str) -> Option<Connection> {
    let db_path = Path::new(base_path).join(DB_FILE);
    let meta = std::fs::symlink_metadata(&db_path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(&db_path, flags).ok()?;
    conn.busy_timeout(std::time::Duration::from_secs(1)).ok()?;
    Some(conn)
}

fn load_thread_rows(conn: &Connection, project_id: Option<&str>) -> Result<Vec<ThreadRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT
                t.id,
                t.title,
                t.workspace_id,
                w.path,
                w.name,
                COALESCE(w.is_temporary, 1),
                t.created_at,
                t.updated_at,
                MIN(m.timestamp) AS first_message_time,
                MAX(m.timestamp) AS last_message_time,
                COUNT(m.id) AS message_count,
                SUM(CASE WHEN m.message LIKE '%\"type\":\"tool-%' OR m.message LIKE '%\"type\": \"tool-%' THEN 1 ELSE 0 END) AS tool_count,
                SUM(CASE WHEN m.message LIKE '%\"state\":\"output-error\"%' OR m.message LIKE '%\"state\": \"output-error\"%' THEN 1 ELSE 0 END) AS error_count
             FROM chat_threads t
             LEFT JOIN workspaces w ON w.id = t.workspace_id
             LEFT JOIN chat_messages m ON m.thread_id = t.id
             GROUP BY t.id
             ORDER BY t.updated_at DESC",
        )
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map([], |row| {
            Ok(ThreadRow {
                id: row.get(0)?,
                title: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                workspace_id: row.get(2)?,
                workspace_path: row.get(3)?,
                workspace_name: row.get(4)?,
                is_temporary: row.get::<_, i64>(5).unwrap_or(1) != 0,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
                first_message_time: row.get(8)?,
                last_message_time: row.get(9)?,
                message_count: usize::try_from(row.get::<_, i64>(10)?).unwrap_or(0),
                has_tool_use: row.get::<_, i64>(11).unwrap_or(0) > 0,
                has_errors: row.get::<_, i64>(12).unwrap_or(0) > 0,
            })
        })
        .map_err(|e| e.to_string())?;

    let mut result = Vec::new();
    for row in rows.filter_map(std::result::Result::ok) {
        let (row_project_id, _, _) = project_identity(&row);
        if project_id.map_or(true, |id| id == row_project_id) {
            result.push(row);
        }
    }
    Ok(result)
}

fn load_messages_with_conn(
    conn: &Connection,
    thread_id: &str,
) -> Result<Vec<ClaudeMessage>, String> {
    if !is_safe_storage_id(thread_id) {
        return Err(format!("Invalid Alma thread id: {thread_id}"));
    }

    let thread_model = conn
        .query_row(
            "SELECT model FROM chat_threads WHERE id = ?1",
            [thread_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten();
    let usage_by_message = load_usage_rows(conn, thread_id)?;
    let mut stmt = conn
        .prepare(
            "SELECT id, parent_id, message, timestamp, metadata
             FROM chat_messages
             WHERE thread_id = ?1
             ORDER BY timestamp ASC, id ASC",
        )
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map([thread_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| e.to_string())?;

    let mut messages = Vec::new();
    for row in rows.filter_map(std::result::Result::ok) {
        let (row_id, parent_id, message_json, timestamp, metadata_json) = row;
        let value: Value = match serde_json::from_str(&message_json) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let metadata = serde_json::from_str::<Value>(&metadata_json).unwrap_or(Value::Null);
        let role = value
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
            .to_string();
        let message_id = value.get("id").and_then(Value::as_str).map(String::from);
        let parts = value
            .get("parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (content, tool_use, tool_use_result, has_tool_error) = normalize_parts(&parts);
        let usage = usage_by_message
            .get(&row_id)
            .map(usage_row_to_token_usage)
            .or_else(|| extract_usage_from_metadata(&metadata));

        let mut msg = build_provider_message(
            PROVIDER_ID,
            row_id.clone(),
            thread_id,
            timestamp,
            normalized_message_type(&role),
            Some(&role),
            content,
            None,
        );
        msg.parent_uuid = parent_id;
        msg.usage = usage;
        msg.tool_use = tool_use;
        msg.tool_use_result = tool_use_result;
        msg.message_id = message_id;
        msg.model = metadata
            .get("model")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| thread_model.clone());
        msg.duration_ms = metadata
            .get("reasoningDuration")
            .and_then(Value::as_f64)
            .map(|seconds| (seconds * 1000.0).max(0.0) as u64);
        msg.stop_reason = metadata
            .get("turnEndReason")
            .and_then(Value::as_str)
            .map(String::from);
        if has_tool_error {
            msg.level = Some("error".to_string());
        }

        messages.push(msg);
    }

    Ok(messages)
}

fn load_usage_rows(
    conn: &Connection,
    thread_id: &str,
) -> Result<std::collections::HashMap<String, UsageRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT message_id, input_tokens, output_tokens, cached_input_tokens, cache_write_input_tokens
             FROM usage_records
             WHERE thread_id = ?1",
        )
        .map_err(|e| e.to_string())?;

    let rows = stmt
        .query_map([thread_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                UsageRow {
                    input: optional_u32(row.get::<_, Option<i64>>(1)?),
                    output: optional_u32(row.get::<_, Option<i64>>(2)?),
                    cache_read: optional_u32(row.get::<_, Option<i64>>(3)?),
                    cache_write: optional_u32(row.get::<_, Option<i64>>(4)?),
                },
            ))
        })
        .map_err(|e| e.to_string())?;

    Ok(rows.filter_map(std::result::Result::ok).collect())
}

fn normalize_parts(parts: &[Value]) -> (Option<Value>, Option<Value>, Option<Value>, bool) {
    let mut content_items = Vec::new();
    let mut first_tool_use = None;
    let mut first_tool_result = None;
    let mut has_tool_error = false;

    for part in parts {
        let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");
        match part_type {
            "text" => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        content_items.push(json!({ "type": "text", "text": text }));
                    }
                }
            }
            "reasoning" => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        content_items.push(json!({ "type": "thinking", "thinking": text }));
                    }
                }
            }
            "step-start" => {}
            raw if raw.starts_with("tool-") => {
                let tool_name = raw.trim_start_matches("tool-");
                let tool_id = part
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if tool_id.is_empty() {
                    continue;
                }

                let input = part
                    .get("input")
                    .or_else(|| part.get("rawInput"))
                    .cloned()
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                let tool_use = json!({
                    "type": "tool_use",
                    "id": tool_id,
                    "name": tool_name,
                    "input": input
                });
                if first_tool_use.is_none() {
                    first_tool_use = Some(tool_use.clone());
                }
                content_items.push(tool_use);

                let state = part.get("state").and_then(Value::as_str).unwrap_or("");
                let is_error = state == "output-error" || part.get("errorText").is_some();
                let output = part
                    .get("output")
                    .cloned()
                    .or_else(|| part.get("errorText").cloned());
                if let Some(output) = output {
                    let mut tool_result = json!({
                        "type": "tool_result",
                        "tool_use_id": tool_id,
                        "content": output
                    });
                    if is_error {
                        tool_result["is_error"] = Value::Bool(true);
                        has_tool_error = true;
                    }
                    if first_tool_result.is_none() {
                        first_tool_result = Some(tool_result.clone());
                    }
                    content_items.push(tool_result);
                }
            }
            _ => {}
        }
    }

    let content = if content_items.is_empty() {
        None
    } else {
        Some(Value::Array(content_items))
    };
    (content, first_tool_use, first_tool_result, has_tool_error)
}

fn project_identity(row: &ThreadRow) -> (String, String, String) {
    if let Some(workspace_id) = row.workspace_id.as_deref() {
        if !row.is_temporary && is_safe_storage_id(workspace_id) {
            let actual_path = row.workspace_path.clone().unwrap_or_default();
            let name = row
                .workspace_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .or_else(|| {
                    Path::new(&actual_path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(String::from)
                })
                .unwrap_or_else(|| "Alma Workspace".to_string());
            return (workspace_id.to_string(), name, actual_path);
        }
    }

    (
        TEMP_PROJECT_ID.to_string(),
        "Alma Temporary Chats".to_string(),
        row.workspace_path.clone().unwrap_or_default(),
    )
}

fn parse_workspace_project_path(project_path: &str) -> Option<&str> {
    let project_id = project_path.strip_prefix("alma://workspace/")?;
    if is_safe_storage_id(project_id) {
        Some(project_id)
    } else {
        None
    }
}

fn parse_thread_session_path(session_path: &str) -> Option<&str> {
    let thread_id = session_path.strip_prefix("alma://thread/")?;
    if is_safe_storage_id(thread_id) {
        Some(thread_id)
    } else {
        None
    }
}

fn normalized_message_type(role: &str) -> &str {
    match role {
        "assistant" => "assistant",
        "system" => "system",
        _ => "user",
    }
}

fn usage_row_to_token_usage(row: &UsageRow) -> TokenUsage {
    let cached_input = row.cache_read.unwrap_or(0);
    let cache_write_input = row.cache_write.unwrap_or(0);
    let cached_total = cached_input.saturating_add(cache_write_input);
    TokenUsage {
        input_tokens: row.input.map(|input| input.saturating_sub(cached_total)),
        output_tokens: row.output,
        cache_creation_input_tokens: row.cache_write,
        cache_read_input_tokens: row.cache_read,
        service_tier: None,
    }
}

fn extract_usage_from_metadata(metadata: &Value) -> Option<TokenUsage> {
    let usage = metadata.get("usage")?;
    let input_tokens = usage
        .get("inputTokens")
        .and_then(Value::as_u64)
        .and_then(to_u32);
    let cache_read_input_tokens = usage
        .get("cachedInputTokens")
        .and_then(Value::as_u64)
        .and_then(to_u32);
    let cache_creation_input_tokens = usage
        .get("cacheWriteInputTokens")
        .or_else(|| usage.get("cacheWriteTokens"))
        .and_then(Value::as_u64)
        .and_then(to_u32);
    let cached_input = cache_read_input_tokens.unwrap_or(0);
    let cache_write_input = cache_creation_input_tokens.unwrap_or(0);
    let cached_total = cached_input.saturating_add(cache_write_input);

    Some(TokenUsage {
        input_tokens: input_tokens.map(|input| input.saturating_sub(cached_total)),
        output_tokens: usage
            .get("outputTokens")
            .and_then(Value::as_u64)
            .and_then(to_u32),
        cache_creation_input_tokens,
        cache_read_input_tokens,
        service_tier: None,
    })
}

fn optional_u32(value: Option<i64>) -> Option<u32> {
    value.and_then(|v| u32::try_from(v).ok())
}

fn to_u32(value: u64) -> Option<u32> {
    u32::try_from(value).ok()
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn escape_like_pattern(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn parses_virtual_paths_safely() {
        assert_eq!(
            parse_workspace_project_path("alma://workspace/abc-123"),
            Some("abc-123")
        );
        assert_eq!(
            parse_thread_session_path("alma://thread/thread_1"),
            Some("thread_1")
        );
        assert_eq!(parse_workspace_project_path("alma://workspace/../x"), None);
        assert_eq!(parse_thread_session_path("alma://thread/a/b"), None);
    }

    #[test]
    fn normalizes_alma_parts_to_claude_content_blocks() {
        let parts = vec![
            json!({ "type": "text", "text": "hello" }),
            json!({ "type": "reasoning", "text": "thinking" }),
            json!({
                "type": "tool-Bash",
                "toolCallId": "call_1",
                "state": "output-available",
                "input": { "command": "echo ok" },
                "output": { "stdout": "ok" }
            }),
        ];

        let (content, tool_use, tool_result, has_error) = normalize_parts(&parts);
        let items = content.unwrap().as_array().cloned().unwrap();

        assert_eq!(items[0]["type"], "text");
        assert_eq!(items[1]["type"], "thinking");
        assert_eq!(items[2]["type"], "tool_use");
        assert_eq!(items[3]["type"], "tool_result");
        assert_eq!(tool_use.unwrap()["name"], "Bash");
        assert_eq!(tool_result.unwrap()["tool_use_id"], "call_1");
        assert!(!has_error);
    }

    #[test]
    fn includes_macos_application_support_before_legacy_home_dir() {
        let home = Path::new("/Users/alice");
        let candidates = home_base_path_candidates(home);

        assert_eq!(
            candidates[0],
            home.join("Library")
                .join("Application Support")
                .join("alma")
        );
        assert_eq!(candidates[1], home.join(".alma"));
    }

    #[test]
    fn scans_alma_fixture_projects_sessions_and_messages() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join(DB_FILE);
        let conn = Connection::open(&db_path).expect("db");
        conn.execute_batch(
            r#"
            CREATE TABLE workspaces (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                name TEXT NOT NULL,
                is_temporary INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE chat_threads (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                model TEXT,
                metadata TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                workspace_id TEXT
            );
            CREATE TABLE chat_messages (
                id TEXT PRIMARY KEY,
                thread_id TEXT NOT NULL,
                parent_id TEXT,
                slot_id TEXT,
                depth INTEGER NOT NULL DEFAULT 0,
                message TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                metadata TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                parent_tool_call_id TEXT
            );
            CREATE TABLE usage_records (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                model TEXT,
                provider_id TEXT,
                date TEXT NOT NULL,
                input_tokens INTEGER DEFAULT 0,
                output_tokens INTEGER DEFAULT 0,
                cached_input_tokens INTEGER DEFAULT 0,
                cache_write_input_tokens INTEGER DEFAULT 0,
                reasoning_tokens INTEGER DEFAULT 0,
                total_tokens INTEGER DEFAULT 0,
                timestamp TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            INSERT INTO workspaces VALUES (
                'ws1', '/tmp/project', 'Project', 0, '2026-05-01T00:00:00.000Z', '2026-05-01T00:00:00.000Z'
            );
            INSERT INTO chat_threads VALUES (
                'thread1', 'Hello Alma', 'model-x', '{}',
                '2026-05-01T00:00:00.000Z', '2026-05-01T00:02:00.000Z', 'ws1'
            );
            INSERT INTO chat_messages VALUES (
                'msg1', 'thread1', NULL, NULL, 0,
                '{"id":"user-1","role":"user","parts":[{"type":"text","text":"Hi"}]}',
                '2026-05-01T00:00:10.000Z', '{}', '2026-05-01T00:00:10.000Z', '2026-05-01T00:00:10.000Z', NULL
            );
            INSERT INTO chat_messages VALUES (
                'msg2', 'thread1', 'msg1', NULL, 1,
                '{"id":"assistant-1","role":"assistant","parts":[{"type":"text","text":"Hello"},{"type":"tool-Bash","toolCallId":"call_1","state":"output-available","input":{"command":"pwd"},"output":{"stdout":"/tmp/project"}}]}',
                '2026-05-01T00:00:20.000Z', '{"usage":{"inputTokens":1,"outputTokens":2}}',
                '2026-05-01T00:00:20.000Z', '2026-05-01T00:00:20.000Z', NULL
            );
            INSERT INTO usage_records VALUES (
                'usage1', 'msg2', 'thread1', 'model-x', 'provider-x', '2026-05-01',
                10, 20, 3, 4, 5, 37, '2026-05-01T00:00:20.000Z', '2026-05-01T00:00:21.000Z'
            );
            "#,
        )
        .expect("fixture");

        let base = dir.path().to_string_lossy().to_string();
        let projects = scan_projects_from_path(&base).expect("projects");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].path, "alma://workspace/ws1");
        assert_eq!(projects[0].session_count, 1);

        let conn = open_db(&base).expect("open read-only");
        let messages = load_messages_with_conn(&conn, "thread1").expect("messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].provider.as_deref(), Some(PROVIDER_ID));
        let usage = messages[1].usage.as_ref().expect("usage");
        assert_eq!(usage.input_tokens, Some(3));
        assert_eq!(usage.cache_read_input_tokens, Some(3));
        assert_eq!(usage.cache_creation_input_tokens, Some(4));
        assert_eq!(usage.output_tokens, Some(20));
        assert!(messages[1].tool_use.is_some());
    }
}
