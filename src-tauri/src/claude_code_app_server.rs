use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
#[cfg(unix)]
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

pub const PROVIDER_NAME: &str = "claude-code";

const PROTOCOL_VERSION: &str = "2025-11-25";
const BIN_ENV: &str = "CODEXL_CLAUDE_CODE_BIN";
const BASE_ARGS_ENV: &str = "CODEXL_CLAUDE_CODE_BASE_ARGS";
const EXTRA_ARGS_ENV: &str = "CODEXL_CLAUDE_CODE_EXTRA_ARGS";
const MODEL_ENV: &str = "CODEXL_CLAUDE_CODE_MODEL";
const PERMISSION_MODE_ENV: &str = "CODEXL_CLAUDE_CODE_PERMISSION_MODE";
const TURN_IDLE_TIMEOUT_MS_ENV: &str = "CODEXL_CLAUDE_CODE_TURN_IDLE_TIMEOUT_MS";
const CLAUDE_PATH_ENV: &str = "CLAUDE_PATH";
const CLAUDE_PATH_OVERRIDE_ENV: &str = "CODEXL_CLAUDE_PATH";
const DEFAULT_MODEL: &str = "claude-code";
const DEFAULT_TURN_IDLE_TIMEOUT_MS: u64 = 5_000;
const MIN_NATIVE_CLAUDE_BYTES: u64 = 5 * 1024 * 1024;
const CLAUDE_STREAM_JSON_ARGS: &[&str] = &[
    "--output-format",
    "stream-json",
    "--verbose",
    "--input-format",
    "stream-json",
    "--include-partial-messages",
];

type SharedOutput<W> = Arc<Mutex<W>>;
type SharedState = Arc<Mutex<ClaudeAppServerState>>;

#[derive(Debug, Clone)]
struct RunOptions {
    workspace_name: Option<String>,
}

#[derive(Debug)]
struct ClaudeAppServerState {
    active_processes: BTreeMap<(String, String), u32>,
    interrupted_turns: BTreeSet<(String, String)>,
    threads: BTreeMap<String, ClaudeThread>,
    workspace_name: Option<String>,
}

#[derive(Debug, Clone)]
struct ClaudeThread {
    id: String,
    session_id: String,
    claude_session_id: String,
    path: Option<String>,
    preview: String,
    cwd: String,
    model: String,
    created_at: i64,
    updated_at: i64,
    archived: bool,
    name: Option<String>,
    turns: Vec<ClaudeTurn>,
}

#[derive(Debug, Clone)]
struct ClaudeTurn {
    id: String,
    input: Vec<Value>,
    tool_items: Vec<Value>,
    agent_text: String,
    status: TurnStatus,
    error: Option<String>,
    started_at: i64,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnStatus {
    InProgress,
    Completed,
    Interrupted,
    Failed,
}

#[derive(Debug)]
struct TurnWork {
    thread_id: String,
    turn_id: String,
    agent_item_id: String,
    cli_item_id: String,
    claude_session_id: String,
    cwd: String,
    prompt: String,
    resume_existing: bool,
}

#[derive(Debug)]
struct ClaudeRunResult {
    text: String,
    error: Option<String>,
    duration_ms: i64,
    tool_items: Vec<Value>,
    agent_item_streamed: bool,
}

#[derive(Debug, Default)]
struct ClaudeStreamState {
    emitted_text: String,
    pending_agent_text: String,
    suppressed_agent_prefix: String,
    result_text: Option<String>,
    result_error: Option<String>,
    agent_item_started: bool,
    reasoning_item_started: bool,
    reasoning_text: String,
    saw_tool_call: bool,
    seen_tool_ids: BTreeSet<String>,
    tool_block_by_index: BTreeMap<i64, String>,
    tool_input_deltas: BTreeMap<String, String>,
    tool_calls: BTreeMap<String, ClaudeToolCallState>,
    completed_tool_ids: BTreeSet<String>,
    completed_tool_items: Vec<Value>,
}

#[derive(Debug, Clone)]
struct ClaudeToolCallState {
    name: String,
    arguments: Value,
    started_at_ms: i64,
    started_emitted: bool,
    kind: ClaudeToolItemKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeToolItemKind {
    CommandExecution,
    McpToolCall,
}

pub fn run_stdio_app_server(args: Vec<OsString>) -> Result<i32, String> {
    run_stdio_app_server_with_io(args, std::io::stdin(), std::io::stdout())
}

pub(crate) fn run_stdio_app_server_with_io<R, W>(
    args: Vec<OsString>,
    input: R,
    output: W,
) -> Result<i32, String>
where
    R: Read,
    W: Write + Send + 'static,
{
    let options = parse_options(args);
    let state = Arc::new(Mutex::new(ClaudeAppServerState {
        active_processes: BTreeMap::new(),
        interrupted_turns: BTreeSet::new(),
        threads: BTreeMap::new(),
        workspace_name: options.workspace_name,
    }));
    let output = Arc::new(Mutex::new(output));
    let mut workers = Vec::new();
    let mut reader = BufReader::new(input);
    let mut line = Vec::new();

    loop {
        line.clear();
        let size = reader
            .read_until(b'\n', &mut line)
            .map_err(|err| format!("failed to read app-server stdin: {}", err))?;
        if size == 0 {
            break;
        }
        if let Some(worker) = handle_client_line(&line, Arc::clone(&state), Arc::clone(&output))? {
            workers.push(worker);
        }
    }

    for worker in workers {
        worker
            .join()
            .map_err(|_| "claude-code turn worker panicked".to_string())?;
    }
    Ok(0)
}

fn parse_options(args: Vec<OsString>) -> RunOptions {
    let mut workspace_name = None;
    let args = args
        .into_iter()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--workspace-name" => {
                index += 1;
                workspace_name = args.get(index).map(|value| value.trim().to_string());
            }
            _ => {}
        }
        index += 1;
    }
    RunOptions { workspace_name }
}

fn handle_client_line<W>(
    line: &[u8],
    state: SharedState,
    output: SharedOutput<W>,
) -> Result<Option<thread::JoinHandle<()>>, String>
where
    W: Write + Send + 'static,
{
    let value = match serde_json::from_slice::<Value>(trim_json_line(line)) {
        Ok(value) => value,
        Err(err) => {
            eprintln!(
                "[codexl-claude-code] ignoring invalid JSON-RPC line: {}",
                err
            );
            return Ok(None);
        }
    };

    let method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if method == "notifications/initialized" || method == "initialized" {
        return Ok(None);
    }
    let id = value.get("id").cloned().unwrap_or(Value::Null);
    let params = value.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let protocol_version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(PROTOCOL_VERSION);
            write_response(
                &output,
                id,
                json!({
                    "protocolVersion": protocol_version,
                    "capabilities": { "experimentalApi": true },
                    "serverInfo": {
                        "name": "codexl-claude-code-app-server",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "userAgent": format!("codexl-claude-code-app-server/{}", env!("CARGO_PKG_VERSION")),
                    "codexHome": crate::config::default_codex_home(),
                    "platformFamily": std::env::consts::FAMILY,
                    "platformOs": std::env::consts::OS,
                }),
            )?;
        }
        "thread/start" => {
            let (response, notification) = {
                let mut state = lock_state(&state)?;
                state.start_thread(&params)
            };
            write_response(&output, id, response)?;
            write_notification(&output, notification)?;
        }
        "thread/resume" => {
            let (response, notification) = {
                let mut state = lock_state(&state)?;
                state.resume_thread(&params)?
            };
            write_response(&output, id, response)?;
            write_notification(&output, notification)?;
        }
        "thread/read" => {
            let response = {
                let state = lock_state(&state)?;
                state.thread_read(&params)?
            };
            write_response(&output, id, response)?;
        }
        "thread/list" => {
            let response = {
                let state = lock_state(&state)?;
                state.thread_list(&params)
            };
            write_response(&output, id, response)?;
        }
        "thread/loaded/list" => {
            let response = {
                let state = lock_state(&state)?;
                json!({
                    "data": state.threads.keys().cloned().collect::<Vec<_>>(),
                    "nextCursor": Value::Null,
                })
            };
            write_response(&output, id, response)?;
        }
        "thread/turns/list" => {
            let response = {
                let state = lock_state(&state)?;
                state.thread_turns_list(&params)?
            };
            write_response(&output, id, response)?;
        }
        "thread/turns/items/list" => {
            write_response(
                &output,
                id,
                json!({ "data": [], "nextCursor": Value::Null }),
            )?;
        }
        "thread/archive" => {
            let notification = {
                let mut state = lock_state(&state)?;
                state.set_archived(&params, true)
            };
            write_response(&output, id, json!({}))?;
            if let Some(notification) = notification {
                write_notification(&output, notification)?;
            }
        }
        "thread/unarchive" => {
            let notification = {
                let mut state = lock_state(&state)?;
                state.set_archived(&params, false)
            };
            write_response(&output, id, json!({}))?;
            if let Some(notification) = notification {
                write_notification(&output, notification)?;
            }
        }
        "thread/unsubscribe" => {
            write_response(&output, id, json!({ "status": "notSubscribed" }))?;
        }
        "thread/name/set" => {
            let notification = {
                let mut state = lock_state(&state)?;
                state.set_thread_name(&params)
            };
            write_response(&output, id, json!({}))?;
            if let Some(notification) = notification {
                write_notification(&output, notification)?;
            }
        }
        "thread/metadata/update" => {
            let response = {
                let state = lock_state(&state)?;
                state.thread_read(&params)?
            };
            write_response(&output, id, response)?;
        }
        "turn/start" => {
            let (response, notifications, work) = {
                let mut state = lock_state(&state)?;
                state.start_turn(&params)?
            };
            write_response(&output, id, response)?;
            for notification in notifications {
                write_notification(&output, notification)?;
            }
            let worker_state = Arc::clone(&state);
            let worker_output = Arc::clone(&output);
            return Ok(Some(thread::spawn(move || {
                run_turn_worker(work, worker_state, worker_output);
            })));
        }
        "turn/interrupt" => {
            let pid = {
                let mut state = lock_state(&state)?;
                state.interrupt_turn(&params)
            };
            write_response(&output, id, json!({}))?;
            if let Some(pid) = pid {
                terminate_process_group(pid);
            }
        }
        "model/list" => {
            write_response(
                &output,
                id,
                json!({ "data": [], "nextCursor": Value::Null }),
            )?;
        }
        "modelProvider/capabilities/read" => {
            write_response(
                &output,
                id,
                json!({
                    "namespaceTools": false,
                    "imageGeneration": false,
                    "webSearch": false,
                }),
            )?;
        }
        "account/read" => {
            write_response(
                &output,
                id,
                json!({ "account": Value::Null, "requiresOpenaiAuth": false }),
            )?;
        }
        "getAuthStatus" => {
            write_response(
                &output,
                id,
                json!({
                    "authMethod": Value::Null,
                    "authToken": Value::Null,
                    "requiresOpenaiAuth": false,
                }),
            )?;
        }
        "permissionProfile/list"
        | "skills/list"
        | "plugin/list"
        | "app/list"
        | "mcpServerStatus/list"
        | "experimentalFeature/list" => {
            write_response(
                &output,
                id,
                json!({ "data": [], "nextCursor": Value::Null }),
            )?;
        }
        "hooks/list" => {
            write_response(&output, id, json!({ "data": [] }))?;
        }
        "collaborationMode/list" => {
            write_response(&output, id, json!({ "data": [] }))?;
        }
        "config/read" => {
            write_response(&output, id, config_read_response(&params))?;
        }
        "configRequirements/read" => {
            write_response(&output, id, json!({ "requirements": Value::Null }))?;
        }
        "config/mcpServer/reload" | "memory/reset" => {
            write_response(&output, id, json!({}))?;
        }
        _ => {
            write_error(
                &output,
                id,
                -32601,
                format!("Claude Code app-server does not support method: {}", method),
            )?;
        }
    }
    Ok(None)
}

impl ClaudeAppServerState {
    fn start_thread(&mut self, params: &Value) -> (Value, Value) {
        let id = new_uuid_v4();
        let cwd = normalize_cwd(params.get("cwd").and_then(Value::as_str));
        let model = model_from_params(params);
        let now = now_seconds();
        let name = self.workspace_name.clone();
        let thread = ClaudeThread {
            id: id.clone(),
            session_id: id.clone(),
            claude_session_id: id,
            path: None,
            preview: String::new(),
            cwd,
            model,
            created_at: now,
            updated_at: now,
            archived: false,
            name,
            turns: Vec::new(),
        };
        let response = thread_runtime_response(&thread, false);
        let notification = json!({
            "method": "thread/started",
            "params": { "thread": thread.to_json(false) },
        });
        self.threads.insert(thread.id.clone(), thread);
        (response, notification)
    }

    fn resume_thread(&mut self, params: &Value) -> Result<(Value, Value), String> {
        let thread_id = params
            .get("threadId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(new_uuid_v4);
        let lookup_thread_id = strip_local_thread_prefix(&thread_id);
        if !self.threads.contains_key(&thread_id) && !self.threads.contains_key(lookup_thread_id) {
            let thread = load_claude_thread_from_params(params, self.workspace_name.clone())
                .or_else(|| load_claude_thread_by_id(lookup_thread_id, self.workspace_name.clone()))
                .ok_or_else(|| format!("thread not found: {}", thread_id))?;
            self.threads.insert(thread.id.clone(), thread);
        }
        let thread = self
            .threads
            .get(&thread_id)
            .or_else(|| self.threads.get(lookup_thread_id))
            .or_else(|| {
                self.threads.values().find(|thread| {
                    thread.path.as_deref()
                        == params
                            .get("path")
                            .and_then(Value::as_str)
                            .filter(|path| !path.trim().is_empty())
                })
            })
            .ok_or_else(|| format!("thread not loaded: {}", thread_id))?;
        let include_turns = !params
            .get("excludeTurns")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let response = thread_runtime_response(thread, include_turns);
        let notification = json!({
            "method": "thread/started",
            "params": { "thread": thread.to_json(false) },
        });
        Ok((response, notification))
    }

    fn thread_read(&self, params: &Value) -> Result<Value, String> {
        let thread_id = required_param(params, "threadId")?;
        let lookup_thread_id = strip_local_thread_prefix(thread_id);
        let include_turns = params
            .get("includeTurns")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if let Some(thread) = self
            .threads
            .get(thread_id)
            .or_else(|| self.threads.get(lookup_thread_id))
        {
            return Ok(json!({ "thread": thread.to_json(include_turns) }));
        }
        let thread = load_claude_thread_by_id(lookup_thread_id, self.workspace_name.clone())
            .ok_or_else(|| format!("thread not found: {}", thread_id))?;
        Ok(json!({ "thread": thread.to_json(include_turns) }))
    }

    fn thread_list(&self, params: &Value) -> Value {
        let archived = params
            .get("archived")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut threads = load_claude_threads(self.workspace_name.clone());
        for thread in self.threads.values() {
            threads.insert(thread.id.clone(), thread.clone());
        }
        let mut data = threads
            .values()
            .filter(|thread| thread_matches_list_params(thread, params, archived))
            .map(|thread| thread.to_json(false))
            .collect::<Vec<_>>();
        let sort_key = match params.get("sortKey").and_then(Value::as_str) {
            Some("created_at") | Some("createdAt") => "createdAt",
            Some("updated_at") | Some("updatedAt") => "updatedAt",
            _ => "createdAt",
        };
        let sort_desc = !matches!(
            params.get("sortDirection").and_then(Value::as_str),
            Some("asc")
        );
        data.sort_by(|left, right| {
            let left_key = left.get(sort_key).and_then(Value::as_i64);
            let right_key = right.get(sort_key).and_then(Value::as_i64);
            if sort_desc {
                right_key.cmp(&left_key)
            } else {
                left_key.cmp(&right_key)
            }
        });
        if let Some(limit) = params.get("limit").and_then(Value::as_u64) {
            data.truncate(limit as usize);
        }
        json!({
            "data": data,
            "nextCursor": Value::Null,
            "backwardsCursor": Value::Null,
        })
    }

    fn thread_turns_list(&self, params: &Value) -> Result<Value, String> {
        let thread_id = required_param(params, "threadId")?;
        let lookup_thread_id = strip_local_thread_prefix(thread_id);
        let loaded_thread;
        let thread = if let Some(thread) = self
            .threads
            .get(thread_id)
            .or_else(|| self.threads.get(lookup_thread_id))
        {
            thread
        } else {
            loaded_thread = load_claude_thread_by_id(lookup_thread_id, self.workspace_name.clone())
                .ok_or_else(|| format!("thread not found: {}", thread_id))?;
            &loaded_thread
        };
        let mut turns = thread.turns.clone();
        if !matches!(
            params.get("sortDirection").and_then(Value::as_str),
            Some("asc")
        ) {
            turns.reverse();
        }
        if let Some(limit) = params.get("limit").and_then(Value::as_u64) {
            turns.truncate(limit as usize);
        }
        Ok(json!({
            "data": turns.iter().map(|turn| turn.to_json(true)).collect::<Vec<_>>(),
            "nextCursor": Value::Null,
            "backwardsCursor": Value::Null,
        }))
    }

    fn set_archived(&mut self, params: &Value, archived: bool) -> Option<Value> {
        let thread_id = params.get("threadId").and_then(Value::as_str)?;
        let thread = self.threads.get_mut(thread_id)?;
        thread.archived = archived;
        thread.updated_at = now_seconds();
        Some(json!({
            "method": if archived { "thread/archived" } else { "thread/unarchived" },
            "params": { "threadId": thread_id },
        }))
    }

    fn set_thread_name(&mut self, params: &Value) -> Option<Value> {
        let thread_id = params.get("threadId").and_then(Value::as_str)?;
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        let thread = self.threads.get_mut(thread_id)?;
        thread.name = name.clone();
        thread.updated_at = now_seconds();
        Some(json!({
            "method": "thread/name/updated",
            "params": {
                "threadId": thread_id,
                "name": name,
            },
        }))
    }

    fn start_turn(&mut self, params: &Value) -> Result<(Value, Vec<Value>, TurnWork), String> {
        let thread_id = required_param(params, "threadId")?.to_string();
        let thread = self
            .threads
            .get_mut(&thread_id)
            .ok_or_else(|| format!("thread not found: {}", thread_id))?;
        if let Some(cwd) = params.get("cwd").and_then(Value::as_str) {
            thread.cwd = normalize_cwd(Some(cwd));
        }
        if let Some(model) = params.get("model").and_then(Value::as_str) {
            thread.model = model.to_string();
        }
        let input = params
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let prompt = prompt_from_input(&input);
        if thread.preview.is_empty() {
            thread.preview = prompt.chars().take(160).collect();
        }
        let now = now_seconds();
        let resume_existing = thread.turns.iter().any(|turn| {
            matches!(
                turn.status,
                TurnStatus::Completed | TurnStatus::Interrupted | TurnStatus::Failed
            )
        });
        let turn = ClaudeTurn {
            id: format!("turn-{}", new_uuid_v4()),
            input,
            tool_items: Vec::new(),
            agent_text: String::new(),
            status: TurnStatus::InProgress,
            error: None,
            started_at: now,
            completed_at: None,
            duration_ms: None,
        };
        let turn_id = turn.id.clone();
        let user_item = turn.user_item_json();
        let agent_item_id = agent_item_id_for_turn(&turn_id);
        let cli_item_id = cli_item_id_for_turn(&turn_id);
        let response_turn = turn.to_json(false);
        thread.updated_at = now;
        thread.turns.push(turn);
        let work = TurnWork {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            agent_item_id,
            cli_item_id,
            claude_session_id: thread.claude_session_id.clone(),
            cwd: thread.cwd.clone(),
            prompt,
            resume_existing,
        };
        Ok((
            json!({ "turn": response_turn.clone() }),
            vec![
                json!({
                    "method": "turn/started",
                    "params": {
                        "threadId": thread_id,
                        "turn": response_turn,
                    },
                }),
                json!({
                    "method": "item/started",
                    "params": {
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "item": user_item,
                        "startedAtMs": now_millis(),
                    },
                }),
            ],
            work,
        ))
    }

    fn interrupt_turn(&mut self, params: &Value) -> Option<u32> {
        let thread_id = params.get("threadId").and_then(Value::as_str)?;
        let turn_id = params.get("turnId").and_then(Value::as_str)?;
        let thread = self.threads.get_mut(thread_id)?;
        let turn = thread.turns.iter_mut().find(|turn| turn.id == turn_id)?;
        turn.status = TurnStatus::Interrupted;
        thread.updated_at = now_seconds();
        let key = (thread_id.to_string(), turn_id.to_string());
        self.interrupted_turns.insert(key.clone());
        self.active_processes.get(&key).copied()
    }

    fn finish_turn(
        &mut self,
        thread_id: &str,
        turn_id: &str,
        result: ClaudeRunResult,
    ) -> Option<(Option<Value>, Value)> {
        let thread = self.threads.get_mut(thread_id)?;
        let turn = thread.turns.iter_mut().find(|turn| turn.id == turn_id)?;
        let key = (thread_id.to_string(), turn_id.to_string());
        self.active_processes.remove(&key);
        let interrupted =
            self.interrupted_turns.remove(&key) || turn.status == TurnStatus::Interrupted;
        turn.tool_items = result.tool_items;
        let agent_item_streamed = result.agent_item_streamed;
        turn.agent_text = result.text;
        turn.duration_ms = Some(result.duration_ms);
        turn.completed_at = Some(now_seconds());
        if interrupted {
            turn.status = TurnStatus::Interrupted;
            turn.error = None;
        } else if let Some(error) = result.error {
            turn.status = TurnStatus::Failed;
            turn.error = Some(error);
        } else {
            turn.status = TurnStatus::Completed;
            turn.error = None;
        }
        thread.updated_at = turn.completed_at.unwrap_or_else(now_seconds);
        let item =
            (!agent_item_streamed && !turn.agent_text.is_empty()).then(|| turn.agent_item_json());
        let turn_json = turn.to_json(false);
        Some((
            item.map(|item| {
                json!({
                    "method": "item/completed",
                    "params": {
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "item": item,
                        "completedAtMs": now_millis(),
                    },
                })
            }),
            json!({
                "method": "turn/completed",
                "params": {
                    "threadId": thread_id,
                    "turn": turn_json,
                },
            }),
        ))
    }
}

impl ClaudeThread {
    fn to_json(&self, include_turns: bool) -> Value {
        json!({
            "id": self.id,
            "sessionId": self.session_id,
            "forkedFromId": Value::Null,
            "preview": self.preview,
            "ephemeral": false,
            "modelProvider": PROVIDER_NAME,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
            "status": self.status_json(),
            "path": self
                .path
                .as_ref()
                .map(|path| Value::String(path.clone()))
                .unwrap_or(Value::Null),
            "cwd": self.cwd,
            "cliVersion": env!("CARGO_PKG_VERSION"),
            "source": "cli",
            "threadSource": Value::Null,
            "agentNickname": Value::Null,
            "agentRole": Value::Null,
            "gitInfo": Value::Null,
            "name": self.name,
            "turns": if include_turns {
                Value::Array(self.turns.iter().map(|turn| turn.to_json(true)).collect())
            } else {
                json!([])
            },
        })
    }

    fn status_json(&self) -> Value {
        if self
            .turns
            .iter()
            .any(|turn| turn.status == TurnStatus::InProgress)
        {
            json!({ "type": "active", "activeFlags": [] })
        } else {
            json!({ "type": "idle" })
        }
    }
}

impl ClaudeTurn {
    fn to_json(&self, include_items: bool) -> Value {
        json!({
            "id": self.id,
            "items": if include_items { self.items_json() } else { json!([]) },
            "itemsView": if include_items { "full" } else { "notLoaded" },
            "status": self.status.as_protocol_str(),
            "error": self.error.as_ref().map(|message| {
                json!({
                    "message": message,
                    "codexErrorInfo": Value::Null,
                    "additionalDetails": Value::Null,
                })
            }),
            "startedAt": self.started_at,
            "completedAt": self.completed_at,
            "durationMs": self.duration_ms,
        })
    }

    fn items_json(&self) -> Value {
        let mut items = Vec::new();
        items.push(json!({
            "type": "userMessage",
            "id": user_item_id_for_turn(&self.id),
            "content": self.input,
        }));
        items.extend(self.tool_items.iter().cloned());
        if !self.agent_text.is_empty() {
            items.push(self.agent_item_json());
        }
        Value::Array(items)
    }

    fn user_item_json(&self) -> Value {
        json!({
            "type": "userMessage",
            "id": user_item_id_for_turn(&self.id),
            "content": self.input,
        })
    }

    fn agent_item_json(&self) -> Value {
        json!({
            "type": "agentMessage",
            "id": agent_item_id_for_turn(&self.id),
            "text": self.agent_text,
            "phase": Value::Null,
            "memoryCitation": Value::Null,
        })
    }
}

impl TurnStatus {
    fn as_protocol_str(self) -> &'static str {
        match self {
            Self::InProgress => "inProgress",
            Self::Completed => "completed",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
        }
    }
}

fn thread_matches_list_params(thread: &ClaudeThread, params: &Value, archived: bool) -> bool {
    if thread.archived != archived {
        return false;
    }
    if let Some(source_kinds) = params.get("sourceKinds").and_then(Value::as_array) {
        if !source_kinds.is_empty()
            && !source_kinds
                .iter()
                .filter_map(Value::as_str)
                .any(|source| source == "cli")
        {
            return false;
        }
    }
    if let Some(providers) = params.get("modelProviders").and_then(Value::as_array) {
        if !providers.is_empty()
            && !providers
                .iter()
                .filter_map(Value::as_str)
                .any(|provider| provider == PROVIDER_NAME)
        {
            return false;
        }
    }
    if let Some(cwd_filter) = params.get("cwd") {
        let cwd_matches = match cwd_filter {
            Value::String(cwd) => cwd == &thread.cwd,
            Value::Array(cwds) => cwds
                .iter()
                .filter_map(Value::as_str)
                .any(|cwd| cwd == thread.cwd),
            _ => true,
        };
        if !cwd_matches {
            return false;
        }
    }
    if let Some(search) = params
        .get("searchTerm")
        .and_then(Value::as_str)
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty())
    {
        let haystack = format!(
            "{}\n{}\n{}",
            thread.preview,
            thread.name.clone().unwrap_or_default(),
            thread.cwd
        )
        .to_ascii_lowercase();
        if !haystack.contains(&search) {
            return false;
        }
    }
    true
}

fn load_claude_thread_from_params(
    params: &Value,
    workspace_name: Option<String>,
) -> Option<ClaudeThread> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())?;
    if !is_claude_transcript_path(Path::new(path)) {
        return None;
    }
    let mut thread = load_claude_thread_from_transcript_path(Path::new(path), workspace_name)?;
    if let Some(cwd) = params
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.trim().is_empty())
    {
        thread.cwd = normalize_cwd(Some(cwd));
    }
    Some(thread)
}

fn load_claude_thread_by_id(
    thread_id: &str,
    workspace_name: Option<String>,
) -> Option<ClaudeThread> {
    let thread_id = strip_local_thread_prefix(thread_id);
    claude_transcript_files()
        .into_iter()
        .filter(|path| path.file_stem().and_then(|value| value.to_str()) == Some(thread_id))
        .filter_map(|path| load_claude_thread_from_transcript_path(&path, workspace_name.clone()))
        .max_by_key(|thread| thread.updated_at)
}

fn strip_local_thread_prefix(thread_id: &str) -> &str {
    thread_id.strip_prefix("local:").unwrap_or(thread_id)
}

fn load_claude_threads(workspace_name: Option<String>) -> BTreeMap<String, ClaudeThread> {
    let mut threads = BTreeMap::new();
    for path in claude_transcript_files() {
        if let Some(thread) = load_claude_thread_from_transcript_path(&path, workspace_name.clone())
        {
            threads
                .entry(thread.id.clone())
                .and_modify(|existing: &mut ClaudeThread| {
                    if thread.updated_at > existing.updated_at {
                        *existing = thread.clone();
                    }
                })
                .or_insert(thread);
        }
    }
    threads
}

fn load_claude_thread_from_transcript_path(
    path: &Path,
    workspace_name: Option<String>,
) -> Option<ClaudeThread> {
    let transcript = std::fs::read_to_string(path).ok()?;
    let metadata = std::fs::metadata(path).ok();
    let fallback_updated_at = metadata
        .as_ref()
        .and_then(|metadata| metadata.modified().ok())
        .and_then(system_time_to_unix_seconds)
        .unwrap_or_else(now_seconds);
    let fallback_created_at = metadata
        .as_ref()
        .and_then(|metadata| metadata.created().ok())
        .and_then(system_time_to_unix_seconds)
        .unwrap_or(fallback_updated_at);
    let path_session_id = path.file_stem()?.to_string_lossy().to_string();

    let mut session_id = path_session_id.clone();
    let mut cwd = String::new();
    let mut model = DEFAULT_MODEL.to_string();
    let mut preview = String::new();
    let mut created_at = fallback_created_at;
    let mut updated_at = fallback_updated_at;
    let mut pending_user: Option<(Vec<Value>, i64)> = None;
    let mut turns = Vec::new();

    for value in transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        if let Some(entry_session_id) = value.get("sessionId").and_then(Value::as_str) {
            if !entry_session_id.trim().is_empty() {
                session_id = entry_session_id.to_string();
            }
        }
        if let Some(entry_cwd) = value.get("cwd").and_then(Value::as_str) {
            if !entry_cwd.trim().is_empty() {
                cwd = entry_cwd.to_string();
            }
        }
        if let Some(timestamp) = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_seconds)
        {
            created_at = created_at.min(timestamp);
            updated_at = updated_at.max(timestamp);
        }
        match value.get("type").and_then(Value::as_str) {
            Some("user")
                if !value
                    .get("isSidechain")
                    .and_then(Value::as_bool)
                    .unwrap_or(false) =>
            {
                if let Some(input) = user_input_from_transcript_entry(&value) {
                    if preview.is_empty() {
                        preview = prompt_from_input(&input).chars().take(160).collect();
                    }
                    let started_at = value
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(parse_rfc3339_seconds)
                        .unwrap_or(updated_at);
                    pending_user = Some((input, started_at));
                }
            }
            Some("assistant")
                if !value
                    .get("isSidechain")
                    .and_then(Value::as_bool)
                    .unwrap_or(false) =>
            {
                if let Some(assistant_model) = value
                    .get("message")
                    .and_then(|message| message.get("model"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty() && *value != "<synthetic>")
                {
                    model = assistant_model.to_string();
                }
                if let (Some((input, started_at)), Some(agent_text)) = (
                    pending_user.take(),
                    assistant_text_from_transcript_entry(&value),
                ) {
                    let completed_at = value
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(parse_rfc3339_seconds)
                        .unwrap_or(updated_at);
                    let failed = value.get("error").and_then(Value::as_str).is_some()
                        || value
                            .get("isApiErrorMessage")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                    let turn_suffix = value
                        .get("uuid")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| turns.len().to_string());
                    turns.push(ClaudeTurn {
                        id: format!("turn-{turn_suffix}"),
                        input,
                        tool_items: Vec::new(),
                        agent_text,
                        status: if failed {
                            TurnStatus::Failed
                        } else {
                            TurnStatus::Completed
                        },
                        error: failed.then(|| {
                            value
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("Claude Code turn failed")
                                .to_string()
                        }),
                        started_at,
                        completed_at: Some(completed_at),
                        duration_ms: Some((completed_at - started_at).max(0) * 1000),
                    });
                }
            }
            Some("last-prompt") => {
                if let Some(last_prompt) = value.get("lastPrompt").and_then(Value::as_str) {
                    if !last_prompt.trim().is_empty() {
                        preview = last_prompt.chars().take(160).collect();
                    }
                }
            }
            _ => {}
        }
    }

    if cwd.is_empty() {
        cwd = cwd_from_claude_project_dir(path).unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .to_string_lossy()
                .to_string()
        });
    }
    if preview.is_empty() {
        preview = session_id.clone();
    }

    Some(ClaudeThread {
        id: session_id.clone(),
        session_id: session_id.clone(),
        claude_session_id: session_id,
        path: Some(path.to_string_lossy().to_string()),
        preview,
        cwd,
        model,
        created_at,
        updated_at,
        archived: false,
        name: workspace_name,
        turns,
    })
}

fn user_input_from_transcript_entry(value: &Value) -> Option<Vec<Value>> {
    let content = value.get("message")?.get("content")?;
    let mut items = Vec::new();
    collect_user_input_items(content, &mut items);
    (!items.is_empty()).then_some(items)
}

fn collect_user_input_items(value: &Value, items: &mut Vec<Value>) {
    match value {
        Value::String(text) => push_text_user_input(items, text),
        Value::Array(parts) => {
            for part in parts {
                collect_user_input_items(part, items);
            }
        }
        Value::Object(map) => {
            if matches!(map.get("type").and_then(Value::as_str), Some("text")) {
                if let Some(text) = map.get("text").and_then(Value::as_str) {
                    push_text_user_input(items, text);
                }
            }
        }
        _ => {}
    }
}

fn push_text_user_input(items: &mut Vec<Value>, text: &str) {
    if text.trim().is_empty() || is_synthetic_user_message(text) {
        return;
    }
    items.push(json!({
        "type": "text",
        "text": text,
        "text_elements": [],
    }));
}

fn is_synthetic_user_message(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.is_empty()
        || trimmed.starts_with("<local-command-stdout>")
        || trimmed.starts_with("<local-command-stderr>")
}

fn claude_transcript_files() -> Vec<PathBuf> {
    let Some(projects_dir) = claude_projects_dir() else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    if let Ok(project_dirs) = std::fs::read_dir(projects_dir) {
        for project_dir in project_dirs.flatten() {
            let project_path = project_dir.path();
            if !project_path.is_dir() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(project_path) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
                        paths.push(path);
                    }
                }
            }
        }
    }
    paths
}

fn claude_projects_dir() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join(".claude")
            .join("projects"),
    )
}

fn is_claude_transcript_path(path: &Path) -> bool {
    let Some(projects_dir) = claude_projects_dir() else {
        return false;
    };
    let Ok(path) = std::fs::canonicalize(path) else {
        return false;
    };
    let Ok(projects_dir) = std::fs::canonicalize(projects_dir) else {
        return false;
    };
    path.starts_with(projects_dir)
        && path.extension().and_then(|value| value.to_str()) == Some("jsonl")
}

fn cwd_from_claude_project_dir(path: &Path) -> Option<String> {
    path.parent()?
        .file_name()?
        .to_str()
        .map(|name| name.replace('-', "/"))
        .filter(|cwd| cwd.starts_with('/'))
}

fn system_time_to_unix_seconds(value: SystemTime) -> Option<i64> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

fn parse_rfc3339_seconds(value: &str) -> Option<i64> {
    let value = value.trim();
    let date_time = value.get(0..19)?;
    let year = date_time.get(0..4)?.parse::<i32>().ok()?;
    let month = date_time.get(5..7)?.parse::<u32>().ok()?;
    let day = date_time.get(8..10)?.parse::<u32>().ok()?;
    let hour = date_time.get(11..13)?.parse::<i64>().ok()?;
    let minute = date_time.get(14..16)?.parse::<i64>().ok()?;
    let second = date_time.get(17..19)?.parse::<i64>().ok()?;
    if date_time.as_bytes().get(4) != Some(&b'-')
        || date_time.as_bytes().get(7) != Some(&b'-')
        || date_time.as_bytes().get(10) != Some(&b'T')
        || date_time.as_bytes().get(13) != Some(&b':')
        || date_time.as_bytes().get(16) != Some(&b':')
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i32;
    let day = day as i32;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era * 146_097 + doe - 719_468)
}

fn run_turn_worker<W>(work: TurnWork, state: SharedState, output: SharedOutput<W>)
where
    W: Write + Send + 'static,
{
    let result = run_claude_code_turn(&work, Arc::clone(&state), Arc::clone(&output));
    let notifications = match lock_state(&state)
        .ok()
        .and_then(|mut state| state.finish_turn(&work.thread_id, &work.turn_id, result))
    {
        Some(notifications) => notifications,
        None => return,
    };
    if let Some(item_completed) = notifications.0 {
        let _ = write_notification(&output, item_completed);
    }
    let _ = write_notification(&output, notifications.1);
}

fn run_claude_code_turn<W>(
    work: &TurnWork,
    state: SharedState,
    output: SharedOutput<W>,
) -> ClaudeRunResult
where
    W: Write,
{
    let started = Instant::now();
    let mut command = claude_command(work);
    command.current_dir(&work.cwd);
    run_claude_code_turn_stream_json(command, work, state, output, started)
}

fn run_claude_code_turn_stream_json<W>(
    mut command: Command,
    work: &TurnWork,
    state: SharedState,
    output: SharedOutput<W>,
    started: Instant,
) -> ClaudeRunResult
where
    W: Write,
{
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        command.process_group(0);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return ClaudeRunResult {
                text: String::new(),
                error: Some(format!("failed to launch Claude Code: {}", err)),
                duration_ms: elapsed_millis(started),
                tool_items: Vec::new(),
                agent_item_streamed: false,
            };
        }
    };
    if let Ok(mut state) = lock_state(&state) {
        state
            .active_processes
            .insert((work.thread_id.clone(), work.turn_id.clone()), child.id());
    }

    let mut child_stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            terminate_process_group(child.id());
            let _ = child.wait();
            remove_active_process(&state, work);
            return ClaudeRunResult {
                text: String::new(),
                error: Some("failed to open Claude Code stdin".to_string()),
                duration_ms: elapsed_millis(started),
                tool_items: Vec::new(),
                agent_item_streamed: false,
            };
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_process_group(child.id());
            let _ = child.wait();
            remove_active_process(&state, work);
            return ClaudeRunResult {
                text: String::new(),
                error: Some("failed to capture Claude Code stdout".to_string()),
                duration_ms: elapsed_millis(started),
                tool_items: Vec::new(),
                agent_item_streamed: false,
            };
        }
    };
    let stderr_handle = child.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut text = String::new();
            let _ = reader.read_to_string(&mut text);
            text
        })
    });

    let stdin_payload = claude_stream_json_input(work);
    if let Err(err) = child_stdin
        .write_all(stdin_payload.as_bytes())
        .and_then(|_| child_stdin.flush())
    {
        terminate_process_group(child.id());
        let _ = child.wait();
        remove_active_process(&state, work);
        return ClaudeRunResult {
            text: String::new(),
            error: Some(format!(
                "failed to write prompt to Claude Code stdin: {}",
                err
            )),
            duration_ms: elapsed_millis(started),
            tool_items: Vec::new(),
            agent_item_streamed: false,
        };
    }
    drop(child_stdin);

    let mut stream = ClaudeStreamState::default();
    let mut command_output = String::new();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(trimmed) {
                    Ok(message) => handle_claude_stream_message(
                        &message,
                        work,
                        &output,
                        &mut stream,
                        &mut command_output,
                    ),
                    Err(_) => {
                        command_output.push_str(&line);
                    }
                }
            }
            Err(err) => {
                terminate_process_group(child.id());
                let _ = child.wait();
                remove_active_process(&state, work);
                finalize_open_tool_calls(&output, work, &mut stream, false);
                let agent_item_streamed = !stream.emitted_text.is_empty();
                return ClaudeRunResult {
                    text: stream.emitted_text,
                    error: Some(format!("failed to read Claude Code stdout: {}", err)),
                    duration_ms: elapsed_millis(started),
                    tool_items: stream.completed_tool_items,
                    agent_item_streamed,
                };
            }
        }
    }

    let status = child.wait();
    remove_active_process(&state, work);
    let stderr = stderr_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    if !stderr.trim().is_empty() {
        command_output.push_str("[stderr]\n");
        command_output.push_str(stderr.trim());
        command_output.push('\n');
    }
    if stream.saw_tool_call {
        flush_pending_agent_text_as_reasoning(&output, work, &mut stream);
    } else {
        flush_pending_agent_text_as_agent(&output, work, &mut stream);
    }
    emit_reasoning_completed_if_started(&output, work, &stream);

    let duration_ms = elapsed_millis(started);
    match status {
        Ok(status) => {
            let success = status.success() && stream.result_error.is_none();
            finalize_open_tool_calls(&output, work, &mut stream, success);
            let agent_item_streamed = !stream.emitted_text.is_empty();
            let final_text = if stream.emitted_text.is_empty() {
                stream
                    .result_text
                    .clone()
                    .or_else(|| latest_claude_transcript_assistant_text(work))
                    .unwrap_or_default()
            } else {
                stream.emitted_text.clone()
            };
            if success {
                ClaudeRunResult {
                    text: final_text,
                    error: None,
                    duration_ms,
                    tool_items: stream.completed_tool_items,
                    agent_item_streamed,
                }
            } else {
                ClaudeRunResult {
                    text: final_text,
                    error: Some(non_empty_join(
                        &[
                            (!status.success())
                                .then(|| format!("Claude Code exited with status {}", status))
                                .unwrap_or_default(),
                            stream.result_error.unwrap_or_default(),
                            command_output,
                            stderr,
                        ],
                        "\n",
                    )),
                    duration_ms,
                    tool_items: stream.completed_tool_items,
                    agent_item_streamed,
                }
            }
        }
        Err(err) => {
            finalize_open_tool_calls(&output, work, &mut stream, false);
            let agent_item_streamed = !stream.emitted_text.is_empty();
            ClaudeRunResult {
                text: stream.emitted_text,
                error: Some(format!("failed to wait for Claude Code: {}", err)),
                duration_ms,
                tool_items: stream.completed_tool_items,
                agent_item_streamed,
            }
        }
    }
}

fn claude_stream_json_input(work: &TurnWork) -> String {
    let initialize = json!({
        "type": "control_request",
        "request_id": new_uuid_v4(),
        "request": { "subtype": "initialize" },
    });
    let user_message = json!({
        "type": "user",
        "session_id": "",
        "message": {
            "role": "user",
            "content": [
                {
                    "type": "text",
                    "text": work.prompt,
                }
            ],
        },
        "parent_tool_use_id": Value::Null,
    });
    format!(
        "{}\n{}\n",
        serde_json::to_string(&initialize).unwrap_or_default(),
        serde_json::to_string(&user_message).unwrap_or_default()
    )
}

fn handle_claude_stream_message<W>(
    message: &Value,
    work: &TurnWork,
    output: &SharedOutput<W>,
    stream: &mut ClaudeStreamState,
    command_output: &mut String,
) where
    W: Write,
{
    match message.get("type").and_then(Value::as_str) {
        Some("stream_event") => {
            if let Some(event) = message.get("event") {
                handle_claude_stream_event(event, message, work, output, stream, command_output);
            }
        }
        Some("assistant") => {
            handle_claude_assistant_message(message, work, output, stream, command_output);
        }
        Some("result") => {
            handle_claude_result_message(message, work, output, stream, command_output);
        }
        Some("user") => {
            handle_claude_user_message(message, work, output, stream, command_output);
        }
        Some("tool_progress") => {}
        Some("tool_use_summary") => {}
        Some("system") => {}
        Some("control_response") | Some("keep_alive") => {}
        Some(other) => {
            let _ = other;
        }
        None => {}
    }
}

fn handle_claude_stream_event<W>(
    event: &Value,
    envelope: &Value,
    work: &TurnWork,
    output: &SharedOutput<W>,
    stream: &mut ClaudeStreamState,
    command_output: &mut String,
) where
    W: Write,
{
    let parent_tool_use_id = envelope.get("parent_tool_use_id").and_then(Value::as_str);
    match event.get("type").and_then(Value::as_str) {
        Some("content_block_start") => {
            let index = event.get("index").and_then(Value::as_i64);
            if let Some(content_block) = event.get("content_block") {
                if let (Some(index), Some(tool_id)) =
                    (index, content_block.get("id").and_then(Value::as_str))
                {
                    stream
                        .tool_block_by_index
                        .insert(index, tool_id.to_string());
                }
                handle_claude_content_block(
                    content_block,
                    parent_tool_use_id,
                    work,
                    output,
                    stream,
                    command_output,
                );
            }
        }
        Some("content_block_delta") => {
            if let Some(delta) = event.get("delta") {
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if parent_tool_use_id.is_none() {
                            if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                handle_claude_agent_text_delta(output, work, stream, text);
                            }
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                            emit_reasoning_delta(output, work, stream, text);
                        }
                    }
                    Some("input_json_delta") => {
                        if let (Some(index), Some(partial_json)) = (
                            event.get("index").and_then(Value::as_i64),
                            delta.get("partial_json").and_then(Value::as_str),
                        ) {
                            if let Some(tool_id) = stream.tool_block_by_index.get(&index) {
                                stream
                                    .tool_input_deltas
                                    .entry(tool_id.clone())
                                    .or_default()
                                    .push_str(partial_json);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Some("content_block_stop") => {
            if let Some(index) = event.get("index").and_then(Value::as_i64) {
                if let Some(tool_id) = stream.tool_block_by_index.get(&index).cloned() {
                    if let Some(input) = stream.tool_input_deltas.remove(&tool_id) {
                        if !input.trim().is_empty() {
                            update_tool_call_arguments(
                                output,
                                work,
                                stream,
                                &tool_id,
                                parse_tool_arguments(input.trim()),
                            );
                        }
                    }
                }
            }
        }
        Some("message_start") | Some("message_delta") | Some("message_stop") => {}
        _ => {}
    }
}

fn handle_claude_assistant_message<W>(
    message: &Value,
    work: &TurnWork,
    output: &SharedOutput<W>,
    stream: &mut ClaudeStreamState,
    command_output: &mut String,
) where
    W: Write,
{
    let parent_tool_use_id = message.get("parent_tool_use_id").and_then(Value::as_str);
    let Some(message_body) = message.get("message") else {
        return;
    };
    if let Some(content) = message_body.get("content") {
        if parent_tool_use_id.is_none() {
            if let Some(text) = claude_text_from_content(content) {
                let Some(text) = visible_agent_snapshot_text(stream, &text) else {
                    return;
                };
                if !stream.saw_tool_call && !stream.agent_item_started {
                    if stream.pending_agent_text.trim() != text.trim() {
                        stream.pending_agent_text = text;
                    }
                    return;
                }
                emit_agent_snapshot(
                    output,
                    &work.thread_id,
                    &work.turn_id,
                    &work.agent_item_id,
                    &mut stream.agent_item_started,
                    &mut stream.emitted_text,
                    &text,
                );
            }
        } else if let Some(text) = claude_text_from_content(content) {
            complete_tool_call(
                output,
                work,
                stream,
                parent_tool_use_id.unwrap_or_default(),
                true,
                Some(text),
            );
        }
        if let Value::Array(items) = content {
            for item in items {
                handle_claude_content_block(
                    item,
                    parent_tool_use_id,
                    work,
                    output,
                    stream,
                    command_output,
                );
            }
        }
    }
}

fn handle_claude_user_message<W>(
    message: &Value,
    work: &TurnWork,
    output: &SharedOutput<W>,
    stream: &mut ClaudeStreamState,
    command_output: &mut String,
) where
    W: Write,
{
    let parent_tool_use_id = message.get("parent_tool_use_id").and_then(Value::as_str);
    let Some(content) = message
        .get("message")
        .and_then(|message| message.get("content"))
    else {
        return;
    };
    match content {
        Value::Array(items) => {
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("tool_result") {
                    handle_claude_content_block(
                        item,
                        parent_tool_use_id,
                        work,
                        output,
                        stream,
                        command_output,
                    );
                }
            }
        }
        Value::Object(_) if content.get("type").and_then(Value::as_str) == Some("tool_result") => {
            handle_claude_content_block(
                content,
                parent_tool_use_id,
                work,
                output,
                stream,
                command_output,
            );
        }
        _ => {}
    }
}

fn handle_claude_result_message<W>(
    message: &Value,
    work: &TurnWork,
    output: &SharedOutput<W>,
    stream: &mut ClaudeStreamState,
    command_output: &mut String,
) where
    W: Write,
{
    if let Some(result) = message.get("result").and_then(Value::as_str) {
        if !result.trim().is_empty() {
            stream.result_text = Some(result.to_string());
        }
    }
    let is_error = message
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if is_error {
        stream.result_error = message
            .get("result")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                message
                    .get("errors")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
            })
            .filter(|text| !text.trim().is_empty());
    }

    let _ = (work, output, command_output);
}

fn handle_claude_content_block<W>(
    block: &Value,
    parent_tool_use_id: Option<&str>,
    work: &TurnWork,
    output: &SharedOutput<W>,
    stream: &mut ClaudeStreamState,
    command_output: &mut String,
) where
    W: Write,
{
    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            if parent_tool_use_id.is_none() {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    let Some(text) = visible_agent_snapshot_text(stream, text) else {
                        return;
                    };
                    if !stream.saw_tool_call && !stream.agent_item_started {
                        if stream.pending_agent_text.trim() != text.trim() {
                            stream.pending_agent_text = text;
                        }
                        return;
                    }
                    emit_agent_snapshot(
                        output,
                        &work.thread_id,
                        &work.turn_id,
                        &work.agent_item_id,
                        &mut stream.agent_item_started,
                        &mut stream.emitted_text,
                        &text,
                    );
                }
            }
        }
        Some("thinking") => {
            if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                emit_reasoning_delta(output, work, stream, text);
            }
        }
        Some("thinking_delta") => {
            if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                emit_reasoning_delta(output, work, stream, text);
            }
        }
        Some("tool_use") | Some("server_tool_use") | Some("mcp_tool_use") => {
            emit_tool_use_event(output, work, stream, command_output, block);
        }
        Some("tool_result")
        | Some("tool_search_tool_result")
        | Some("web_fetch_tool_result")
        | Some("web_search_tool_result")
        | Some("code_execution_tool_result")
        | Some("bash_code_execution_tool_result")
        | Some("text_editor_code_execution_tool_result")
        | Some("mcp_tool_result") => {
            emit_tool_result_event(output, work, stream, command_output, block);
        }
        _ => {}
    }
}

fn handle_claude_agent_text_delta<W>(
    output: &SharedOutput<W>,
    work: &TurnWork,
    stream: &mut ClaudeStreamState,
    delta: &str,
) where
    W: Write,
{
    if delta.is_empty() {
        return;
    }
    if !stream.saw_tool_call && !stream.agent_item_started {
        stream.pending_agent_text.push_str(delta);
        return;
    }
    append_agent_delta(output, work, stream, delta);
}

fn append_agent_delta<W>(
    output: &SharedOutput<W>,
    work: &TurnWork,
    stream: &mut ClaudeStreamState,
    delta: &str,
) where
    W: Write,
{
    if delta.is_empty() {
        return;
    }
    let next_text = format!("{}{}", stream.emitted_text, delta);
    emit_agent_delta(
        output,
        &work.thread_id,
        &work.turn_id,
        &work.agent_item_id,
        &mut stream.agent_item_started,
        &mut stream.emitted_text,
        &next_text,
    );
}

fn flush_pending_agent_text_as_reasoning<W>(
    output: &SharedOutput<W>,
    work: &TurnWork,
    stream: &mut ClaudeStreamState,
) where
    W: Write,
{
    if stream.pending_agent_text.trim().is_empty() {
        stream.pending_agent_text.clear();
        return;
    }
    let text = std::mem::take(&mut stream.pending_agent_text);
    stream.suppressed_agent_prefix.push_str(&text);
    emit_reasoning_delta(output, work, stream, &text);
}

fn flush_pending_agent_text_as_agent<W>(
    output: &SharedOutput<W>,
    work: &TurnWork,
    stream: &mut ClaudeStreamState,
) where
    W: Write,
{
    if stream.pending_agent_text.trim().is_empty() {
        stream.pending_agent_text.clear();
        return;
    }
    let text = std::mem::take(&mut stream.pending_agent_text);
    append_agent_delta(output, work, stream, &text);
}

fn visible_agent_snapshot_text(stream: &ClaudeStreamState, text: &str) -> Option<String> {
    let mut visible = text;
    if !stream.suppressed_agent_prefix.is_empty()
        && visible.starts_with(stream.suppressed_agent_prefix.as_str())
    {
        visible = &visible[stream.suppressed_agent_prefix.len()..];
    }
    let visible = visible
        .trim_start_matches(|ch: char| ch.is_whitespace())
        .to_string();
    (!visible.trim().is_empty()).then_some(visible)
}

fn emit_agent_snapshot<W>(
    output: &SharedOutput<W>,
    thread_id: &str,
    turn_id: &str,
    item_id: &str,
    item_started: &mut bool,
    emitted_text: &mut String,
    next_text: &str,
) where
    W: Write,
{
    if next_text.is_empty() {
        return;
    }
    if emitted_text.trim() == next_text.trim() {
        return;
    }
    if emitted_text.is_empty() || next_text.starts_with(emitted_text.as_str()) {
        emit_agent_delta(
            output,
            thread_id,
            turn_id,
            item_id,
            item_started,
            emitted_text,
            next_text,
        );
    }
}

fn emit_tool_use_event<W>(
    output: &SharedOutput<W>,
    work: &TurnWork,
    stream: &mut ClaudeStreamState,
    command_output: &mut String,
    block: &Value,
) where
    W: Write,
{
    let tool_id = block.get("id").and_then(Value::as_str).unwrap_or("unknown");
    let tool_name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
    stream.seen_tool_ids.insert(tool_id.to_string());
    let arguments = block
        .get("input")
        .filter(|value| !value.is_null())
        .cloned()
        .unwrap_or_else(|| json!({}));
    emit_tool_call_started(output, work, stream, tool_id, tool_name, arguments);
    let _ = command_output;
}

fn emit_tool_result_event<W>(
    output: &SharedOutput<W>,
    work: &TurnWork,
    stream: &mut ClaudeStreamState,
    command_output: &mut String,
    block: &Value,
) where
    W: Write,
{
    let tool_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let status = if block
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        "failed"
    } else {
        "completed"
    };
    let result = block
        .get("content")
        .and_then(claude_text_from_content)
        .unwrap_or_else(|| compact_json(block));
    complete_tool_call(
        output,
        work,
        stream,
        tool_id,
        status == "completed",
        Some(result),
    );
    let _ = command_output;
}

fn emit_tool_call_started<W>(
    output: &SharedOutput<W>,
    work: &TurnWork,
    stream: &mut ClaudeStreamState,
    tool_id: &str,
    tool_name: &str,
    arguments: Value,
) where
    W: Write,
{
    let tool_id = non_empty_string(tool_id).unwrap_or_else(|| "unknown".to_string());
    let tool_name = non_empty_string(tool_name).unwrap_or_else(|| "tool".to_string());
    stream.saw_tool_call = true;
    flush_pending_agent_text_as_reasoning(output, work, stream);
    {
        let entry =
            stream
                .tool_calls
                .entry(tool_id.clone())
                .or_insert_with(|| ClaudeToolCallState {
                    name: tool_name.clone(),
                    arguments: json!({}),
                    started_at_ms: now_millis(),
                    started_emitted: false,
                    kind: claude_tool_item_kind(&tool_name),
                });
        entry.name = tool_name.clone();
        entry.kind = claude_tool_item_kind(&tool_name);
        if !is_empty_tool_arguments(&arguments) {
            entry.arguments = arguments;
        }
    }
    maybe_emit_tool_call_started(output, work, stream, &tool_id, false);
}

fn maybe_emit_tool_call_started<W>(
    output: &SharedOutput<W>,
    work: &TurnWork,
    stream: &mut ClaudeStreamState,
    tool_id: &str,
    force: bool,
) where
    W: Write,
{
    let Some(state) = stream.tool_calls.get_mut(tool_id) else {
        return;
    };
    if state.started_emitted || (!force && is_empty_tool_arguments(&state.arguments)) {
        return;
    }
    let item = tool_call_item(&work.cwd, tool_id, state, "inProgress", None, Value::Null);
    state.started_emitted = true;
    let _ = write_notification(
        output,
        json!({
            "method": "item/started",
            "params": {
                "threadId": work.thread_id,
                "turnId": work.turn_id,
                "item": item,
                "startedAtMs": state.started_at_ms,
            },
        }),
    );
}

fn update_tool_call_arguments<W>(
    output: &SharedOutput<W>,
    work: &TurnWork,
    stream: &mut ClaudeStreamState,
    tool_id: &str,
    arguments: Value,
) where
    W: Write,
{
    let tool_id = non_empty_string(tool_id).unwrap_or_else(|| "unknown".to_string());
    if !stream.tool_calls.contains_key(&tool_id) {
        emit_tool_call_started(output, work, stream, &tool_id, "tool", arguments);
        return;
    }
    if !is_empty_tool_arguments(&arguments) {
        if let Some(state) = stream.tool_calls.get_mut(&tool_id) {
            state.arguments = arguments;
        }
    }
    maybe_emit_tool_call_started(output, work, stream, &tool_id, false);
}

fn complete_tool_call<W>(
    output: &SharedOutput<W>,
    work: &TurnWork,
    stream: &mut ClaudeStreamState,
    tool_id: &str,
    success: bool,
    result: Option<String>,
) where
    W: Write,
{
    let tool_id = non_empty_string(tool_id).unwrap_or_else(|| "unknown".to_string());
    if stream.completed_tool_ids.contains(&tool_id) {
        return;
    }
    if !stream.tool_calls.contains_key(&tool_id) {
        emit_tool_call_started(output, work, stream, &tool_id, "tool", json!({}));
    }
    maybe_emit_tool_call_started(output, work, stream, &tool_id, true);
    let Some(state) = stream.tool_calls.get(&tool_id).cloned() else {
        return;
    };
    stream.completed_tool_ids.insert(tool_id.clone());
    let item = tool_call_item(
        &work.cwd,
        &tool_id,
        &state,
        if success { "completed" } else { "failed" },
        result.as_deref(),
        Value::Null,
    );
    stream.completed_tool_items.push(item.clone());
    let _ = write_notification(
        output,
        json!({
            "method": "item/completed",
            "params": {
                "threadId": work.thread_id,
                "turnId": work.turn_id,
                "item": item,
                "completedAtMs": now_millis(),
            },
        }),
    );
}

fn finalize_open_tool_calls<W>(
    output: &SharedOutput<W>,
    work: &TurnWork,
    stream: &mut ClaudeStreamState,
    success: bool,
) where
    W: Write,
{
    let tool_ids = stream.tool_calls.keys().cloned().collect::<Vec<_>>();
    for tool_id in tool_ids {
        complete_tool_call(output, work, stream, &tool_id, success, None);
    }
}

fn tool_call_item(
    cwd: &str,
    tool_id: &str,
    state: &ClaudeToolCallState,
    status: &str,
    result: Option<&str>,
    duration_ms: Value,
) -> Value {
    match state.kind {
        ClaudeToolItemKind::CommandExecution => {
            command_execution_item_for_tool(tool_id, state, status, result, duration_ms, cwd)
        }
        ClaudeToolItemKind::McpToolCall => mcp_tool_call_item(tool_id, state, status, result),
    }
}

fn claude_tool_item_kind(tool_name: &str) -> ClaudeToolItemKind {
    match tool_name {
        "Bash" => ClaudeToolItemKind::CommandExecution,
        _ => ClaudeToolItemKind::McpToolCall,
    }
}

fn command_execution_item_for_tool(
    tool_id: &str,
    state: &ClaudeToolCallState,
    status: &str,
    result: Option<&str>,
    duration_ms: Value,
    cwd: &str,
) -> Value {
    let command = state
        .arguments
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| compact_json(&state.arguments));
    let aggregated_output = result
        .map(|value| json!(truncate_for_protocol(value, 200_000)))
        .unwrap_or(Value::Null);
    json!({
        "type": "commandExecution",
        "id": tool_item_id(tool_id),
        "command": command,
        "cwd": state.arguments.get("cwd").and_then(Value::as_str).unwrap_or(cwd),
        "processId": Value::Null,
        "source": "agent",
        "status": status,
        "commandActions": [
            {
                "type": "unknown",
                "command": command,
            }
        ],
        "aggregatedOutput": aggregated_output,
        "exitCode": if status == "completed" { json!(0) } else { Value::Null },
        "durationMs": duration_ms,
    })
}

fn mcp_tool_call_item(
    tool_id: &str,
    state: &ClaudeToolCallState,
    status: &str,
    result: Option<&str>,
) -> Value {
    let failed = status == "failed";
    json!({
        "type": "mcpToolCall",
        "id": tool_item_id(tool_id),
        "server": "claude-code",
        "tool": state.name.clone(),
        "status": status,
        "arguments": state.arguments.clone(),
        "pluginId": Value::Null,
        "result": if failed { Value::Null } else { mcp_tool_result(result) },
        "error": if failed {
            json!({ "message": result.unwrap_or("Claude Code tool failed") })
        } else {
            Value::Null
        },
        "durationMs": Value::Null,
    })
}

fn mcp_tool_result(result: Option<&str>) -> Value {
    match result.map(str::trim).filter(|value| !value.is_empty()) {
        Some(result) => json!({
            "content": [{ "type": "text", "text": truncate_for_protocol(result, 20_000) }],
            "structuredContent": Value::Null,
            "_meta": Value::Null,
        }),
        None => Value::Null,
    }
}

fn parse_tool_arguments(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw).unwrap_or_else(|_| json!({ "raw": raw }))
}

fn is_empty_tool_arguments(value: &Value) -> bool {
    value.is_null()
        || value.as_object().is_some_and(|object| object.is_empty())
        || value.as_array().is_some_and(|array| array.is_empty())
        || value.as_str().is_some_and(str::is_empty)
}

fn tool_item_id(tool_id: &str) -> String {
    format!("claude-tool-{}", sanitize_item_id(tool_id))
}

fn sanitize_item_id(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn claude_text_from_content(content: &Value) -> Option<String> {
    let mut parts = Vec::new();
    collect_claude_text(content, &mut parts);
    let text = parts.join("");
    (!text.trim().is_empty()).then_some(text)
}

fn collect_claude_text(value: &Value, parts: &mut Vec<String>) {
    match value {
        Value::String(text) => parts.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_claude_text(item, parts);
            }
        }
        Value::Object(map) => {
            if matches!(
                map.get("type").and_then(Value::as_str),
                Some("text") | Some("text_delta")
            ) {
                if let Some(text) = map.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            } else if let Some(content) = map.get("content") {
                collect_claude_text(content, parts);
            }
        }
        _ => {}
    }
}

fn claude_result_usage_summary(message: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(num_turns) = message.get("num_turns").and_then(Value::as_i64) {
        parts.push(format!("turns={num_turns}"));
    }
    if let Some(cost) = message.get("total_cost_usd").and_then(Value::as_f64) {
        parts.push(format!("cost=${cost:.6}"));
    }
    if let Some(duration_ms) = message.get("duration_ms").and_then(Value::as_i64) {
        parts.push(format!("duration={}ms", duration_ms));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("result {}", parts.join(" "))
    }
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(not(unix))]
fn run_claude_code_turn_piped<W>(
    mut command: Command,
    work: &TurnWork,
    state: SharedState,
    output: SharedOutput<W>,
    started: Instant,
) -> ClaudeRunResult
where
    W: Write,
{
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return ClaudeRunResult {
                text: String::new(),
                error: Some(format!("failed to launch Claude Code: {}", err)),
                duration_ms: elapsed_millis(started),
                tool_items: Vec::new(),
                agent_item_streamed: false,
            };
        }
    };
    emit_command_execution_started(&output, work, Some(child.id()));
    if let Ok(mut state) = lock_state(&state) {
        state
            .active_processes
            .insert((work.thread_id.clone(), work.turn_id.clone()), child.id());
    }

    let mut child_stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            terminate_process_group(child.id());
            let _ = child.wait();
            if let Ok(mut state) = lock_state(&state) {
                state
                    .active_processes
                    .remove(&(work.thread_id.clone(), work.turn_id.clone()));
            }
            return ClaudeRunResult {
                text: String::new(),
                error: Some("failed to open Claude Code stdin".to_string()),
                duration_ms: elapsed_millis(started),
                tool_items: Vec::new(),
            };
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_process_group(child.id());
            let _ = child.wait();
            if let Ok(mut state) = lock_state(&state) {
                state
                    .active_processes
                    .remove(&(work.thread_id.clone(), work.turn_id.clone()));
            }
            return ClaudeRunResult {
                text: String::new(),
                error: Some("failed to capture Claude Code stdout".to_string()),
                duration_ms: elapsed_millis(started),
                tool_items: Vec::new(),
            };
        }
    };
    let stderr_handle = child.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut text = String::new();
            let _ = reader.read_to_string(&mut text);
            text
        })
    });

    if let Err(err) = child_stdin
        .write_all(work.prompt.as_bytes())
        .and_then(|_| child_stdin.write_all(b"\n"))
        .and_then(|_| child_stdin.flush())
    {
        terminate_process_group(child.id());
        let _ = child.wait();
        if let Ok(mut state) = lock_state(&state) {
            state
                .active_processes
                .remove(&(work.thread_id.clone(), work.turn_id.clone()));
        }
        return ClaudeRunResult {
            text: String::new(),
            error: Some(format!(
                "failed to write prompt to Claude Code stdin: {}",
                err
            )),
            duration_ms: elapsed_millis(started),
            tool_items: Vec::new(),
        };
    }
    drop(child_stdin);

    let mut emitted_text = String::new();
    let mut agent_item_started = false;
    let mut command_output = String::new();
    let mut raw_stdout = String::new();
    let mut reader = stdout;
    let mut buffer = [0u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => {
                let chunk = String::from_utf8_lossy(&buffer[..size]).to_string();
                raw_stdout.push_str(&chunk);
                emit_command_execution_output_delta(
                    &output,
                    &work.thread_id,
                    &work.turn_id,
                    &work.cli_item_id,
                    &chunk,
                    &work.prompt,
                    &mut command_output,
                );
                let text = clean_interactive_cli_output(&raw_stdout, &work.prompt);
                emit_agent_delta(
                    &output,
                    &work.thread_id,
                    &work.turn_id,
                    &work.agent_item_id,
                    &mut agent_item_started,
                    &mut emitted_text,
                    &text,
                );
            }
            Err(err) => {
                terminate_process_group(child.id());
                let _ = child.wait();
                if let Ok(mut state) = lock_state(&state) {
                    state
                        .active_processes
                        .remove(&(work.thread_id.clone(), work.turn_id.clone()));
                }
                return ClaudeRunResult {
                    text: emitted_text,
                    error: Some(format!("failed to read Claude Code stdout: {}", err)),
                    duration_ms: elapsed_millis(started),
                    tool_items: Vec::new(),
                    agent_item_streamed: !emitted_text.is_empty(),
                };
            }
        }
    }

    let status = child.wait();
    if let Ok(mut state) = lock_state(&state) {
        state
            .active_processes
            .remove(&(work.thread_id.clone(), work.turn_id.clone()));
    }
    let stderr = stderr_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let cleaned_stdout = clean_interactive_cli_output(&raw_stdout, &work.prompt);
    let cleaned_stderr = clean_interactive_cli_output(&stderr, &work.prompt);
    let final_text = latest_claude_transcript_assistant_text(work).unwrap_or_else(|| {
        if !cleaned_stdout.is_empty() {
            cleaned_stdout.clone()
        } else if !cleaned_stderr.is_empty() {
            cleaned_stderr.clone()
        } else {
            emitted_text.clone()
        }
    });
    if emitted_text.is_empty() && !final_text.is_empty() {
        emit_agent_delta(
            &output,
            &work.thread_id,
            &work.turn_id,
            &work.agent_item_id,
            &mut agent_item_started,
            &mut emitted_text,
            &final_text,
        );
    }

    let duration_ms = elapsed_millis(started);
    match status {
        Ok(status) => {
            emit_command_execution_completed(
                &output,
                work,
                Some(child.id()),
                status.success(),
                &command_output,
                status.code(),
                duration_ms,
            );
            if status.success() {
                ClaudeRunResult {
                    text: if emitted_text.is_empty() {
                        final_text
                    } else {
                        emitted_text
                    },
                    error: None,
                    duration_ms,
                    tool_items: Vec::new(),
                    agent_item_streamed: !emitted_text.is_empty(),
                }
            } else {
                ClaudeRunResult {
                    text: emitted_text,
                    error: Some(non_empty_join(
                        &[
                            format!("Claude Code exited with status {}", status),
                            cleaned_stderr,
                            final_text,
                        ],
                        "\n",
                    )),
                    duration_ms,
                    tool_items: Vec::new(),
                    agent_item_streamed: !emitted_text.is_empty(),
                }
            }
        }
        Err(err) => {
            emit_command_execution_completed(
                &output,
                work,
                Some(child.id()),
                false,
                &command_output,
                None,
                duration_ms,
            );
            ClaudeRunResult {
                text: emitted_text,
                error: Some(format!("failed to wait for Claude Code: {}", err)),
                duration_ms,
                tool_items: Vec::new(),
                agent_item_streamed: !emitted_text.is_empty(),
            }
        }
    }
}

#[cfg(unix)]
fn run_claude_code_turn_pty<W>(
    mut command: Command,
    work: &TurnWork,
    state: SharedState,
    output: SharedOutput<W>,
    started: Instant,
) -> ClaudeRunResult
where
    W: Write,
{
    let (mut master, slave) = match open_unix_pty() {
        Ok(pair) => pair,
        Err(err) => return run_claude_code_turn_piped(command, work, state, output, started, err),
    };

    let stdin = match slave.try_clone() {
        Ok(file) => file,
        Err(err) => return run_claude_code_turn_piped(command, work, state, output, started, err),
    };
    let stdout = match slave.try_clone() {
        Ok(file) => file,
        Err(err) => return run_claude_code_turn_piped(command, work, state, output, started, err),
    };
    let stderr = match slave.try_clone() {
        Ok(file) => file,
        Err(err) => return run_claude_code_turn_piped(command, work, state, output, started, err),
    };

    unsafe {
        command
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .pre_exec(|| {
                for signo in [
                    libc::SIGCHLD,
                    libc::SIGHUP,
                    libc::SIGINT,
                    libc::SIGQUIT,
                    libc::SIGTERM,
                    libc::SIGALRM,
                ] {
                    libc::signal(signo, libc::SIG_DFL);
                }

                let empty_set: libc::sigset_t = std::mem::zeroed();
                libc::sigprocmask(libc::SIG_SETMASK, &empty_set, std::ptr::null_mut());

                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }

                #[allow(clippy::cast_lossless)]
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return ClaudeRunResult {
                text: String::new(),
                error: Some(format!("failed to launch Claude Code: {}", err)),
                duration_ms: elapsed_millis(started),
                tool_items: Vec::new(),
                agent_item_streamed: false,
            };
        }
    };
    drop(slave);

    emit_command_execution_started(&output, work, Some(child.id()));
    if let Ok(mut state) = lock_state(&state) {
        state
            .active_processes
            .insert((work.thread_id.clone(), work.turn_id.clone()), child.id());
    }

    let mut writer = match master.try_clone() {
        Ok(file) => file,
        Err(err) => {
            terminate_process_group(child.id());
            let _ = child.wait();
            remove_active_process(&state, work);
            return ClaudeRunResult {
                text: String::new(),
                error: Some(format!("failed to clone Claude Code PTY: {}", err)),
                duration_ms: elapsed_millis(started),
                tool_items: Vec::new(),
                agent_item_streamed: false,
            };
        }
    };

    if let Err(err) = set_nonblocking(master.as_raw_fd()) {
        terminate_process_group(child.id());
        let _ = child.wait();
        remove_active_process(&state, work);
        return ClaudeRunResult {
            text: String::new(),
            error: Some(format!("failed to configure Claude Code PTY: {}", err)),
            duration_ms: elapsed_millis(started),
            tool_items: Vec::new(),
            agent_item_streamed: false,
        };
    }

    let idle_timeout = claude_turn_idle_timeout();
    let mut exit_requested_at: Option<Instant> = None;
    let mut last_meaningful_output_at = Instant::now();
    let mut trust_confirmed = false;
    let mut trust_confirmed_at: Option<Instant> = None;
    let mut prompt_sent = false;
    let mut saw_turn_content = false;
    let mut last_raw_output_at = Instant::now();
    let mut raw_output = String::new();
    let mut command_output = String::new();
    let mut buffer = [0u8; 4096];
    let status = loop {
        match master.read(&mut buffer) {
            Ok(0) => {
                if let Ok(Some(exit_status)) = child.try_wait() {
                    break Some(exit_status);
                }
                thread::sleep(Duration::from_millis(25));
            }
            Ok(size) => {
                let chunk = String::from_utf8_lossy(&buffer[..size]).to_string();
                last_raw_output_at = Instant::now();
                raw_output.push_str(&chunk);
                if !trust_confirmed && looks_like_claude_trust_prompt(&raw_output) {
                    if let Err(err) = writer.write_all(b"\r").and_then(|_| writer.flush()) {
                        terminate_process_group(child.id());
                        let _ = child.wait();
                        remove_active_process(&state, work);
                        return ClaudeRunResult {
                            text: String::new(),
                            error: Some(format!(
                                "failed to write prompt to Claude Code PTY: {}",
                                err
                            )),
                            duration_ms: elapsed_millis(started),
                            tool_items: Vec::new(),
                            agent_item_streamed: false,
                        };
                    }
                    trust_confirmed = true;
                    trust_confirmed_at = Some(Instant::now());
                }
                let emitted = emit_command_execution_output_delta(
                    &output,
                    &work.thread_id,
                    &work.turn_id,
                    &work.cli_item_id,
                    &chunk,
                    &work.prompt,
                    &mut command_output,
                );
                if emitted && prompt_sent {
                    saw_turn_content = true;
                    last_meaningful_output_at = Instant::now();
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if let Ok(Some(exit_status)) = child.try_wait() {
                    break Some(exit_status);
                }
                match exit_requested_at {
                    Some(requested_at)
                        if requested_at.elapsed() >= Duration::from_millis(1_200) =>
                    {
                        terminate_process_group(child.id());
                        break child.wait().ok();
                    }
                    Some(_) => {}
                    None if !prompt_sent
                        && should_send_prompt_to_claude(
                            started,
                            trust_confirmed_at,
                            &raw_output,
                            last_raw_output_at,
                        ) =>
                    {
                        match writer
                            .write_all(b"\x1b[200~")
                            .and_then(|_| writer.write_all(work.prompt.as_bytes()))
                            .and_then(|_| writer.write_all(b"\x1b[201~\r"))
                            .and_then(|_| writer.flush())
                        {
                            Ok(()) => {
                                prompt_sent = true;
                                last_meaningful_output_at = Instant::now();
                            }
                            Err(err) => {
                                terminate_process_group(child.id());
                                let _ = child.wait();
                                remove_active_process(&state, work);
                                emit_command_execution_completed(
                                    &output,
                                    work,
                                    Some(child.id()),
                                    false,
                                    &command_output,
                                    None,
                                    elapsed_millis(started),
                                );
                                return ClaudeRunResult {
                                    text: String::new(),
                                    error: Some(format!(
                                        "failed to write prompt to Claude Code PTY: {}",
                                        err
                                    )),
                                    duration_ms: elapsed_millis(started),
                                    tool_items: Vec::new(),
                                    agent_item_streamed: false,
                                };
                            }
                        }
                    }
                    None if prompt_sent
                        && saw_turn_content
                        && last_meaningful_output_at.elapsed() >= idle_timeout =>
                    {
                        let _ = writer.write_all(b"/exit\r").and_then(|_| writer.flush());
                        exit_requested_at = Some(Instant::now());
                    }
                    None => {}
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(err) => {
                terminate_process_group(child.id());
                let _ = child.wait();
                remove_active_process(&state, work);
                emit_command_execution_completed(
                    &output,
                    work,
                    Some(child.id()),
                    false,
                    &command_output,
                    None,
                    elapsed_millis(started),
                );
                return ClaudeRunResult {
                    text: String::new(),
                    error: Some(format!("failed to read Claude Code PTY: {}", err)),
                    duration_ms: elapsed_millis(started),
                    tool_items: Vec::new(),
                    agent_item_streamed: false,
                };
            }
        }
    };

    remove_active_process(&state, work);
    let duration_ms = elapsed_millis(started);
    let success = status.as_ref().is_some_and(|status| status.success());
    let exit_code = status.as_ref().and_then(|status| status.code());
    emit_command_execution_completed(
        &output,
        work,
        Some(child.id()),
        success,
        &command_output,
        exit_code,
        duration_ms,
    );

    let final_text = latest_claude_transcript_assistant_text(work)
        .unwrap_or_else(|| clean_interactive_cli_output(&raw_output, &work.prompt));
    let mut emitted_text = String::new();
    let mut agent_item_started = false;
    if !final_text.is_empty() {
        emit_agent_delta(
            &output,
            &work.thread_id,
            &work.turn_id,
            &work.agent_item_id,
            &mut agent_item_started,
            &mut emitted_text,
            &final_text,
        );
    }

    if success {
        ClaudeRunResult {
            text: final_text,
            error: None,
            duration_ms,
            tool_items: Vec::new(),
            agent_item_streamed: !emitted_text.is_empty(),
        }
    } else {
        ClaudeRunResult {
            text: final_text.clone(),
            error: Some(non_empty_join(
                &[
                    status
                        .as_ref()
                        .map(|status| format!("Claude Code exited with status {}", status))
                        .unwrap_or_else(|| "Claude Code did not exit cleanly".to_string()),
                    final_text,
                ],
                "\n",
            )),
            duration_ms,
            tool_items: Vec::new(),
            agent_item_streamed: !emitted_text.is_empty(),
        }
    }
}

#[cfg(unix)]
fn run_claude_code_turn_piped<W>(
    mut command: Command,
    work: &TurnWork,
    state: SharedState,
    output: SharedOutput<W>,
    started: Instant,
    pty_error: std::io::Error,
) -> ClaudeRunResult
where
    W: Write,
{
    eprintln!(
        "[codexl-claude-code] failed to start PTY, falling back to pipes: {}",
        pty_error
    );
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return ClaudeRunResult {
                text: String::new(),
                error: Some(format!("failed to launch Claude Code: {}", err)),
                duration_ms: elapsed_millis(started),
                tool_items: Vec::new(),
                agent_item_streamed: false,
            };
        }
    };
    emit_command_execution_started(&output, work, Some(child.id()));
    if let Ok(mut state) = lock_state(&state) {
        state
            .active_processes
            .insert((work.thread_id.clone(), work.turn_id.clone()), child.id());
    }

    let mut child_stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            terminate_process_group(child.id());
            let _ = child.wait();
            if let Ok(mut state) = lock_state(&state) {
                state
                    .active_processes
                    .remove(&(work.thread_id.clone(), work.turn_id.clone()));
            }
            return ClaudeRunResult {
                text: String::new(),
                error: Some("failed to open Claude Code stdin".to_string()),
                duration_ms: elapsed_millis(started),
                tool_items: Vec::new(),
                agent_item_streamed: false,
            };
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_process_group(child.id());
            let _ = child.wait();
            if let Ok(mut state) = lock_state(&state) {
                state
                    .active_processes
                    .remove(&(work.thread_id.clone(), work.turn_id.clone()));
            }
            return ClaudeRunResult {
                text: String::new(),
                error: Some("failed to capture Claude Code stdout".to_string()),
                duration_ms: elapsed_millis(started),
                tool_items: Vec::new(),
                agent_item_streamed: false,
            };
        }
    };
    let stderr_handle = child.stderr.take().map(|stderr| {
        thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut text = String::new();
            let _ = reader.read_to_string(&mut text);
            text
        })
    });

    if let Err(err) = child_stdin
        .write_all(work.prompt.as_bytes())
        .and_then(|_| child_stdin.write_all(b"\n"))
        .and_then(|_| child_stdin.flush())
    {
        terminate_process_group(child.id());
        let _ = child.wait();
        if let Ok(mut state) = lock_state(&state) {
            state
                .active_processes
                .remove(&(work.thread_id.clone(), work.turn_id.clone()));
        }
        return ClaudeRunResult {
            text: String::new(),
            error: Some(format!(
                "failed to write prompt to Claude Code stdin: {}",
                err
            )),
            duration_ms: elapsed_millis(started),
            tool_items: Vec::new(),
            agent_item_streamed: false,
        };
    }
    drop(child_stdin);

    let mut emitted_text = String::new();
    let mut agent_item_started = false;
    let mut command_output = String::new();
    let mut raw_stdout = String::new();
    let mut reader = stdout;
    let mut buffer = [0u8; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => {
                let chunk = String::from_utf8_lossy(&buffer[..size]).to_string();
                raw_stdout.push_str(&chunk);
                emit_command_execution_output_delta(
                    &output,
                    &work.thread_id,
                    &work.turn_id,
                    &work.cli_item_id,
                    &chunk,
                    &work.prompt,
                    &mut command_output,
                );
                let text = clean_interactive_cli_output(&raw_stdout, &work.prompt);
                emit_agent_delta(
                    &output,
                    &work.thread_id,
                    &work.turn_id,
                    &work.agent_item_id,
                    &mut agent_item_started,
                    &mut emitted_text,
                    &text,
                );
            }
            Err(err) => {
                terminate_process_group(child.id());
                let _ = child.wait();
                if let Ok(mut state) = lock_state(&state) {
                    state
                        .active_processes
                        .remove(&(work.thread_id.clone(), work.turn_id.clone()));
                }
                let agent_item_streamed = !emitted_text.is_empty();
                return ClaudeRunResult {
                    text: emitted_text,
                    error: Some(format!("failed to read Claude Code stdout: {}", err)),
                    duration_ms: elapsed_millis(started),
                    tool_items: Vec::new(),
                    agent_item_streamed,
                };
            }
        }
    }

    let status = child.wait();
    if let Ok(mut state) = lock_state(&state) {
        state
            .active_processes
            .remove(&(work.thread_id.clone(), work.turn_id.clone()));
    }
    let stderr = stderr_handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    let cleaned_stdout = clean_interactive_cli_output(&raw_stdout, &work.prompt);
    let cleaned_stderr = clean_interactive_cli_output(&stderr, &work.prompt);
    let final_text = latest_claude_transcript_assistant_text(work).unwrap_or_else(|| {
        if !cleaned_stdout.is_empty() {
            cleaned_stdout.clone()
        } else if !cleaned_stderr.is_empty() {
            cleaned_stderr.clone()
        } else {
            emitted_text.clone()
        }
    });
    if emitted_text.is_empty() && !final_text.is_empty() {
        emit_agent_delta(
            &output,
            &work.thread_id,
            &work.turn_id,
            &work.agent_item_id,
            &mut agent_item_started,
            &mut emitted_text,
            &final_text,
        );
    }

    let agent_item_streamed = !emitted_text.is_empty();
    let duration_ms = elapsed_millis(started);
    match status {
        Ok(status) => {
            emit_command_execution_completed(
                &output,
                work,
                Some(child.id()),
                status.success(),
                &command_output,
                status.code(),
                duration_ms,
            );
            if status.success() {
                ClaudeRunResult {
                    text: if emitted_text.is_empty() {
                        final_text
                    } else {
                        emitted_text
                    },
                    error: None,
                    duration_ms,
                    tool_items: Vec::new(),
                    agent_item_streamed,
                }
            } else {
                ClaudeRunResult {
                    text: emitted_text,
                    error: Some(non_empty_join(
                        &[
                            format!("Claude Code exited with status {}", status),
                            cleaned_stderr,
                            final_text,
                        ],
                        "\n",
                    )),
                    duration_ms,
                    tool_items: Vec::new(),
                    agent_item_streamed,
                }
            }
        }
        Err(err) => {
            emit_command_execution_completed(
                &output,
                work,
                Some(child.id()),
                false,
                &command_output,
                None,
                duration_ms,
            );
            ClaudeRunResult {
                text: emitted_text,
                error: Some(format!("failed to wait for Claude Code: {}", err)),
                duration_ms,
                tool_items: Vec::new(),
                agent_item_streamed,
            }
        }
    }
}

fn claude_command(work: &TurnWork) -> Command {
    let bin = std::env::var(BIN_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "ccr".to_string());
    let mut command = Command::new(&bin);
    configure_claude_path_env(&mut command, &bin);
    command.env("DISABLE_AUTOUPDATER", "1");
    command.env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts");
    command.env("CLAUDE_CODE_EMIT_SESSION_STATE_EVENTS", "1");
    for arg in env_args(BASE_ARGS_ENV, &["code"]) {
        command.arg(arg);
    }
    for arg in CLAUDE_STREAM_JSON_ARGS {
        command.arg(arg);
    }
    if work.resume_existing {
        command.arg("--resume").arg(&work.claude_session_id);
    } else {
        command.arg("--session-id").arg(&work.claude_session_id);
    }
    if let Some(model) = claude_model_arg() {
        command.arg("--model").arg(model);
    }
    if let Some(permission_mode) = std::env::var(PERMISSION_MODE_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        command.arg("--permission-mode").arg(permission_mode);
    }
    for arg in env_args(EXTRA_ARGS_ENV, &[]) {
        command.arg(arg);
    }
    command
}

fn remove_active_process(state: &SharedState, work: &TurnWork) {
    if let Ok(mut state) = lock_state(state) {
        state
            .active_processes
            .remove(&(work.thread_id.clone(), work.turn_id.clone()));
    }
}

fn claude_turn_idle_timeout() -> Duration {
    std::env::var(TURN_IDLE_TIMEOUT_MS_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(DEFAULT_TURN_IDLE_TIMEOUT_MS))
}

#[cfg(unix)]
fn open_unix_pty() -> std::io::Result<(File, File)> {
    let mut master: RawFd = -1;
    let mut slave: RawFd = -1;
    let mut size = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(size),
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    set_cloexec(master)?;
    set_cloexec(slave)?;
    Ok(unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) })
}

#[cfg(unix)]
fn set_cloexec(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let result = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn set_nonblocking(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error());
    }
    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn claude_model_arg() -> Option<String> {
    std::env::var(MODEL_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value != DEFAULT_MODEL)
}

fn should_send_prompt_to_claude(
    started: Instant,
    trust_confirmed_at: Option<Instant>,
    raw_output: &str,
    last_raw_output_at: Instant,
) -> bool {
    if last_raw_output_at.elapsed() < Duration::from_millis(600) {
        return false;
    }
    let input_ready = looks_like_claude_input_ready(raw_output);
    if let Some(confirmed_at) = trust_confirmed_at {
        return (input_ready && confirmed_at.elapsed() >= Duration::from_millis(500))
            || confirmed_at.elapsed() >= Duration::from_millis(10_000);
    }
    if looks_like_claude_trust_prompt(raw_output) {
        return false;
    }
    (input_ready && started.elapsed() >= Duration::from_millis(500))
        || started.elapsed() >= Duration::from_millis(10_000)
}

fn looks_like_claude_input_ready(raw_output: &str) -> bool {
    let plain = strip_ansi_and_control(raw_output).to_lowercase();
    plain.contains("welcome back")
        || plain.contains("tips for getting started")
        || plain.contains("run /init")
        || plain.contains("/effort")
        || plain.contains("claude code v")
}

fn configure_claude_path_env(command: &mut Command, ccr_bin: &str) {
    if let Some(path) = std::env::var(CLAUDE_PATH_OVERRIDE_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        command.env(CLAUDE_PATH_ENV, path);
        return;
    }

    if std::env::var(CLAUDE_PATH_ENV)
        .ok()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
    {
        return;
    }

    if let Some(path) = resolve_claude_path_for_ccr(ccr_bin) {
        command.env(CLAUDE_PATH_ENV, path);
        command.env("CLAUDE_CODE_INSTALLED_VIA_NPM_WRAPPER", "1");
    }
}

fn resolve_claude_path_for_ccr(ccr_bin: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();

    candidates.extend(resolve_executable_paths("claude"));
    for ccr_path in resolve_executable_paths(ccr_bin) {
        candidates.extend(claude_candidates_near_ccr(&ccr_path));
    }

    let mut seen = BTreeSet::new();
    candidates.into_iter().find(|candidate| {
        let key = candidate.to_string_lossy().to_string();
        seen.insert(key) && is_probably_usable_claude_path(candidate)
    })
}

fn claude_candidates_near_ccr(ccr_path: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let Some(bin_dir) = ccr_path.parent() else {
        return candidates;
    };

    candidates.push(bin_dir.join("claude"));
    candidates.extend(hidden_bin_candidates(bin_dir));

    if let Some(global_modules) = node_global_modules_from_bin_dir(bin_dir) {
        candidates.extend(claude_candidates_from_global_modules(&global_modules));
    }

    candidates
}

fn hidden_bin_candidates(bin_dir: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(bin_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if name.starts_with(".claude") {
                candidates.push(path);
            }
        }
    }
    candidates.sort();
    candidates
}

fn node_global_modules_from_bin_dir(bin_dir: &Path) -> Option<PathBuf> {
    (bin_dir.file_name().and_then(|value| value.to_str()) == Some("bin")).then(|| {
        bin_dir
            .parent()
            .unwrap_or(bin_dir)
            .join("lib")
            .join("node_modules")
    })
}

fn claude_candidates_from_global_modules(global_modules: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let anthropic_dir = global_modules.join("@anthropic-ai");
    for package_dir in anthropic_claude_package_dirs(&anthropic_dir) {
        candidates.push(package_dir.join("bin").join("claude.exe"));
        if let Some(native_package) = native_claude_package_name() {
            candidates.push(
                package_dir
                    .join("node_modules")
                    .join("@anthropic-ai")
                    .join(native_package)
                    .join(if cfg!(windows) {
                        "claude.exe"
                    } else {
                        "claude"
                    }),
            );
        }
    }
    candidates
}

fn anthropic_claude_package_dirs(anthropic_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let primary = anthropic_dir.join("claude-code");
    if primary.is_dir() {
        dirs.push(primary);
    }
    if let Ok(entries) = std::fs::read_dir(anthropic_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if name.starts_with(".claude-code-") && path.is_dir() {
                dirs.push(path);
            }
        }
    }
    dirs.sort();
    dirs
}

fn native_claude_package_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("claude-code-darwin-arm64"),
        ("macos", "x86_64") => Some("claude-code-darwin-x64"),
        ("linux", "x86_64") => Some("claude-code-linux-x64"),
        ("linux", "aarch64") => Some("claude-code-linux-arm64"),
        ("windows", "x86_64") => Some("claude-code-win32-x64"),
        ("windows", "aarch64") => Some("claude-code-win32-arm64"),
        _ => None,
    }
}

fn resolve_executable_paths(program: &str) -> Vec<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return vec![path.to_path_buf()];
    }

    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(program))
                .collect()
        })
        .unwrap_or_default()
}

fn is_probably_usable_claude_path(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() || !is_executable_file(&metadata) {
        return false;
    }
    if is_claude_code_placeholder(path) {
        return false;
    }

    let path_text = path.to_string_lossy();
    if path_text.contains("@anthropic-ai/claude-code") && metadata.len() < MIN_NATIVE_CLAUDE_BYTES {
        return false;
    }

    true
}

fn is_claude_code_placeholder(path: &Path) -> bool {
    let Ok(contents) = std::fs::read(path) else {
        return false;
    };
    let prefix = &contents[..contents.len().min(1024)];
    String::from_utf8_lossy(prefix).contains("claude native binary not installed")
}

#[cfg(unix)]
fn is_executable_file(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable_file(metadata: &std::fs::Metadata) -> bool {
    let _ = metadata;
    true
}

fn env_args(name: &str, default: &[&str]) -> Vec<String> {
    std::env::var(name)
        .ok()
        .map(|value| split_env_args(&value))
        .unwrap_or_else(|| default.iter().map(|value| value.to_string()).collect())
}

fn split_env_args(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn emit_command_execution_started<W>(output: &SharedOutput<W>, work: &TurnWork, pid: Option<u32>)
where
    W: Write,
{
    let _ = write_notification(
        output,
        json!({
            "method": "item/started",
            "params": {
                "threadId": work.thread_id,
                "turnId": work.turn_id,
                "item": command_execution_item(work, pid, "inProgress", Value::Null, Value::Null, Value::Null),
                "startedAtMs": now_millis(),
            },
        }),
    );
}

fn emit_command_execution_completed<W>(
    output: &SharedOutput<W>,
    work: &TurnWork,
    pid: Option<u32>,
    success: bool,
    aggregated_output: &str,
    exit_code: Option<i32>,
    duration_ms: i64,
) where
    W: Write,
{
    let status = if success { "completed" } else { "failed" };
    let _ = write_notification(
        output,
        json!({
            "method": "item/completed",
            "params": {
                "threadId": work.thread_id,
                "turnId": work.turn_id,
                "item": command_execution_item(
                    work,
                    pid,
                    status,
                    json!(truncate_for_protocol(aggregated_output, 200_000)),
                    exit_code.map(Value::from).unwrap_or(Value::Null),
                    json!(duration_ms),
                ),
                "completedAtMs": now_millis(),
            },
        }),
    );
}

fn emit_command_execution_output_delta<W>(
    output: &SharedOutput<W>,
    thread_id: &str,
    turn_id: &str,
    item_id: &str,
    raw_delta: &str,
    prompt: &str,
    aggregated_output: &mut String,
) -> bool
where
    W: Write,
{
    let delta = clean_command_output_delta(raw_delta, prompt);
    if delta.trim().is_empty() {
        return false;
    }
    aggregated_output.push_str(&delta);
    let _ = write_notification(
        output,
        json!({
            "method": "item/commandExecution/outputDelta",
            "params": {
                "threadId": thread_id,
                "turnId": turn_id,
                "itemId": item_id,
                "delta": delta,
            },
        }),
    );
    true
}

fn emit_command_execution_structured_delta<W>(
    output: &SharedOutput<W>,
    thread_id: &str,
    turn_id: &str,
    item_id: &str,
    delta: &str,
    aggregated_output: &mut String,
) -> bool
where
    W: Write,
{
    if delta.trim().is_empty() {
        return false;
    }
    aggregated_output.push_str(delta);
    let _ = write_notification(
        output,
        json!({
            "method": "item/commandExecution/outputDelta",
            "params": {
                "threadId": thread_id,
                "turnId": turn_id,
                "itemId": item_id,
                "delta": delta,
            },
        }),
    );
    true
}

fn command_execution_item(
    work: &TurnWork,
    pid: Option<u32>,
    status: &str,
    aggregated_output: Value,
    exit_code: Value,
    duration_ms: Value,
) -> Value {
    let command = claude_command_display(work);
    json!({
        "type": "commandExecution",
        "id": work.cli_item_id,
        "command": command,
        "cwd": work.cwd,
        "processId": pid.map(|value| value.to_string()),
        "source": "agent",
        "status": status,
        "commandActions": [
            {
                "type": "unknown",
                "command": command,
            }
        ],
        "aggregatedOutput": aggregated_output,
        "exitCode": exit_code,
        "durationMs": duration_ms,
    })
}

fn claude_command_display(work: &TurnWork) -> String {
    let bin = std::env::var(BIN_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "ccr".to_string());
    let mut parts = vec![bin];
    parts.extend(env_args(BASE_ARGS_ENV, &["code"]));
    parts.extend(CLAUDE_STREAM_JSON_ARGS.iter().map(|arg| arg.to_string()));
    if work.resume_existing {
        parts.push("--resume".to_string());
    } else {
        parts.push("--session-id".to_string());
    }
    parts.push(work.claude_session_id.clone());
    if let Some(model) = claude_model_arg() {
        parts.push("--model".to_string());
        parts.push(model);
    }
    if let Some(permission_mode) = std::env::var(PERMISSION_MODE_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        parts.push("--permission-mode".to_string());
        parts.push(permission_mode);
    }
    parts.extend(env_args(EXTRA_ARGS_ENV, &[]));
    parts
        .into_iter()
        .map(|part| shell_display_token(&part))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_display_token(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':' | '='))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn latest_claude_transcript_assistant_text(work: &TurnWork) -> Option<String> {
    let path = claude_transcript_path(work)?;
    let transcript = std::fs::read_to_string(path).ok()?;
    latest_assistant_text_from_transcript(&transcript)
}

fn claude_transcript_path(work: &TurnWork) -> Option<PathBuf> {
    let projects_dir = PathBuf::from(std::env::var_os("HOME")?)
        .join(".claude")
        .join("projects");
    let filename = format!("{}.jsonl", work.claude_session_id);
    for dir_name in claude_project_dir_candidates(&work.cwd) {
        let path = projects_dir.join(dir_name).join(&filename);
        if path.is_file() {
            return Some(path);
        }
    }
    std::fs::read_dir(projects_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(&filename))
        .find(|path| path.is_file())
}

fn claude_project_dir_candidates(cwd: &str) -> Vec<String> {
    let mut paths = Vec::new();
    paths.push(PathBuf::from(cwd));
    if let Ok(canonical) = std::fs::canonicalize(cwd) {
        paths.push(canonical);
    }
    if cwd.starts_with("/var/") {
        paths.push(PathBuf::from(format!("/private{}", cwd)));
    }

    let mut seen = BTreeSet::new();
    paths
        .into_iter()
        .map(|path| claude_project_dir_name(&path))
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

fn claude_project_dir_name(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|ch| {
            if matches!(ch, '/' | '\\' | ':') {
                '-'
            } else {
                ch
            }
        })
        .collect()
}

fn latest_assistant_text_from_transcript(transcript: &str) -> Option<String> {
    transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| assistant_text_from_transcript_entry(&value))
        .last()
}

fn assistant_text_from_transcript_entry(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let message = value.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let content = message.get("content")?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    item.get("text").and_then(Value::as_str)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    };
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn clean_command_output_delta(raw_delta: &str, prompt: &str) -> String {
    let prompt_compact = compact_cli_text(prompt);
    let lines = strip_ansi_and_control(raw_delta)
        .lines()
        .map(normalize_cli_line)
        .filter(|line| !line.is_empty())
        .filter(|line| !is_cli_chrome_line(line))
        .filter(|line| !is_claude_noise_line(line))
        .filter(|line| {
            let compact = compact_plain_text(line);
            !compact.is_empty() && (prompt_compact.is_empty() || !compact.contains(&prompt_compact))
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn looks_like_claude_trust_prompt(raw: &str) -> bool {
    let compact = compact_cli_text(raw);
    compact.contains("quicksafetycheck")
        || compact.contains("yes,itrustthisfolder")
        || compact.contains("no,exit")
}

fn compact_cli_text(raw: &str) -> String {
    compact_plain_text(&strip_ansi_and_control(raw))
}

fn compact_plain_text(raw: &str) -> String {
    raw.chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

fn is_claude_noise_line(line: &str) -> bool {
    let compact = compact_plain_text(line);
    let has_spinner = line.chars().any(|ch| {
        matches!(
            ch,
            '✻' | '✽'
                | '✶'
                | '✳'
                | '✢'
                | '·'
                | '◐'
                | '◓'
                | '◑'
                | '◒'
                | '⠋'
                | '⠙'
                | '⠹'
                | '⠸'
                | '⠼'
                | '⠴'
                | '⠦'
                | '⠧'
                | '⠇'
                | '⠏'
        )
    });
    let has_banner_block = line
        .chars()
        .any(|ch| matches!(ch, '▐' | '▛' | '█' | '▜' | '▌' | '▝' | '▘'));
    let has_status_glyph = line.chars().any(|ch| matches!(ch, '󰉋' | '' | '󰚩' | '⚡'));
    let has_cjk = line.chars().any(is_cjk);
    compact.is_empty()
        || (has_spinner && compact.chars().count() <= 40)
        || (has_spinner && compact.contains("brewing"))
        || (has_spinner && compact.contains("drizzling"))
        || (has_spinner && compact.contains("bakedfor"))
        || (has_banner_block && !has_cjk)
        || (has_status_glyph && !has_cjk)
        || (compact.chars().count() <= 2 && !has_cjk)
        || compact.chars().all(|ch| ch.is_ascii_digit())
        || compact.contains("claudecodev")
        || compact.contains("welcomeback")
        || compact.contains("tipsforgettingstarted")
        || compact.contains("what'snew")
        || compact.contains("opus4")
        || compact.contains("1mcontext")
        || compact.contains("internalinfrastructureimprovements")
        || compact.contains("release-notes")
        || compact.contains("/usage")
        || compact.contains("/diff")
        || compact.contains("apiusagebilling")
        || compact.contains("/effort")
        || compact.contains("tok/s")
        || compact.contains("tokens)")
        || compact.contains("↑")
        || compact.contains("↓")
        || compact.contains("brewing")
        || compact.contains("drizzling")
        || compact.contains("bakedfor")
        || compact.contains("auto-updating")
        || compact.contains("auto-updatefailed")
        || compact.contains("accessingworkspace")
        || compact.contains("quicksafetycheck")
        || compact.contains("securityguide")
        || compact.contains("securityguid")
        || compact.contains("project,orworkfromyourteam")
        || compact.contains("ifnot,takeamomenttoreview")
        || compact.contains("claudecode'llbeabletoread")
        || compact.contains("edit,andexecutefileshere")
        || compact.contains("doyoutrustthefiles")
        || compact.contains("yes,itrustthisfolder")
        || compact.contains("no,exit")
        || compact.contains("entertoconfirm")
        || compact.contains("esctocancel")
        || compact.contains("pressctrl-d")
        || compact.contains("resumethissessionwith:")
        || compact.starts_with("claude--resume")
        || compact.starts_with("ccrcode--resume")
        || compact.contains("codexl-claude-code-")
        || compact.contains("coxl-claude-code-")
}

fn is_cjk(ch: char) -> bool {
    matches!(
        ch,
        '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{2A6DF}'
            | '\u{2A700}'..='\u{2B73F}'
            | '\u{2B740}'..='\u{2B81F}'
            | '\u{2B820}'..='\u{2CEAF}'
            | '\u{3000}'..='\u{303F}'
            | '\u{FF00}'..='\u{FFEF}'
    )
}

fn truncate_for_protocol(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut start = value.len().saturating_sub(max_bytes);
    while !value.is_char_boundary(start) {
        start += 1;
    }
    format!("[truncated]\n{}", &value[start..])
}

fn emit_agent_delta<W>(
    output: &SharedOutput<W>,
    thread_id: &str,
    turn_id: &str,
    item_id: &str,
    item_started: &mut bool,
    emitted_text: &mut String,
    next_text: &str,
) where
    W: Write,
{
    let delta = if next_text.starts_with(emitted_text.as_str()) {
        next_text[emitted_text.len()..].to_string()
    } else if emitted_text.is_empty() {
        next_text.to_string()
    } else if next_text != emitted_text {
        format!("\n\n{}", next_text)
    } else {
        String::new()
    };
    if delta.is_empty() {
        return;
    }
    if !*item_started {
        let _ = write_notification(
            output,
            json!({
                "method": "item/started",
                "params": {
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "item": {
                        "type": "agentMessage",
                        "id": item_id,
                        "text": "",
                        "phase": Value::Null,
                        "memoryCitation": Value::Null,
                    },
                    "startedAtMs": now_millis(),
                },
            }),
        );
        *item_started = true;
    }
    emitted_text.push_str(&delta);
    let _ = write_notification(
        output,
        json!({
            "method": "item/agentMessage/delta",
            "params": {
                "threadId": thread_id,
                "turnId": turn_id,
                "itemId": item_id,
                "delta": delta,
            },
        }),
    );
}

fn emit_reasoning_delta<W>(
    output: &SharedOutput<W>,
    work: &TurnWork,
    stream: &mut ClaudeStreamState,
    delta: &str,
) where
    W: Write,
{
    if delta.is_empty() {
        return;
    }
    let item_id = reasoning_item_id_for_turn(&work.turn_id);
    if !stream.reasoning_item_started {
        let _ = write_notification(
            output,
            json!({
                "method": "item/started",
                "params": {
                    "threadId": work.thread_id,
                    "turnId": work.turn_id,
                    "item": {
                        "type": "reasoning",
                        "id": item_id,
                        "summary": [],
                        "content": [],
                    },
                    "startedAtMs": now_millis(),
                },
            }),
        );
        stream.reasoning_item_started = true;
    }
    stream.reasoning_text.push_str(delta);
    let _ = write_notification(
        output,
        json!({
            "method": "item/reasoning/textDelta",
            "params": {
                "threadId": work.thread_id,
                "turnId": work.turn_id,
                "itemId": item_id,
                "delta": delta,
                "contentIndex": 0,
            },
        }),
    );
}

fn emit_reasoning_completed_if_started<W>(
    output: &SharedOutput<W>,
    work: &TurnWork,
    stream: &ClaudeStreamState,
) where
    W: Write,
{
    if !stream.reasoning_item_started {
        return;
    }
    let item_id = reasoning_item_id_for_turn(&work.turn_id);
    let _ = write_notification(
        output,
        json!({
            "method": "item/completed",
            "params": {
                "threadId": work.thread_id,
                "turnId": work.turn_id,
                "item": {
                    "type": "reasoning",
                    "id": item_id,
                    "summary": [],
                    "content": if stream.reasoning_text.is_empty() {
                        json!([])
                    } else {
                        json!([stream.reasoning_text])
                    },
                },
                "completedAtMs": now_millis(),
            },
        }),
    );
}

fn user_item_id_for_turn(turn_id: &str) -> String {
    format!("user-{}", turn_id)
}

fn agent_item_id_for_turn(turn_id: &str) -> String {
    format!("agent-{}", turn_id)
}

fn reasoning_item_id_for_turn(turn_id: &str) -> String {
    format!("reasoning-{}", turn_id)
}

fn cli_item_id_for_turn(turn_id: &str) -> String {
    format!("claude-cli-{}", turn_id)
}

fn clean_interactive_cli_output(raw: &str, prompt: &str) -> String {
    let plain = strip_ansi_and_control(raw);
    let prompt_compact = compact_cli_text(prompt);
    let prompt_lines = prompt
        .lines()
        .map(normalize_cli_line)
        .filter(|line| !line.is_empty())
        .collect::<BTreeSet<_>>();
    let mut lines = Vec::new();
    for line in plain.lines().map(normalize_cli_line) {
        let compact = compact_plain_text(&line);
        if line.is_empty()
            || prompt_lines.contains(&line)
            || (!prompt_compact.is_empty() && compact.contains(&prompt_compact))
            || is_cli_chrome_line(&line)
            || is_claude_noise_line(&line)
        {
            continue;
        }
        if lines.last().map(String::as_str) != Some(line.as_str()) {
            lines.push(line);
        }
    }
    lines.join("\n")
}

fn normalize_cli_line(line: &str) -> String {
    let normalized = line
        .trim()
        .trim_matches(|ch| matches!(ch, '│' | '┃' | '║' | '╎' | '┆' | '╭' | '╮' | '╰' | '╯'))
        .trim()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    normalized
        .strip_prefix('⏺')
        .map(str::trim)
        .unwrap_or(&normalized)
        .to_string()
}

fn is_cli_chrome_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    line == ">"
        || line == "❯"
        || line == "..."
        || lower == "claude code"
        || lower.contains("esc to interrupt")
        || lower.contains("ctrl+c")
        || lower.contains("ctrl+d")
        || lower.contains("? for shortcuts")
        || lower.contains("press enter")
        || line.chars().all(|ch| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    '─' | '━'
                        | '═'
                        | '╭'
                        | '╮'
                        | '╰'
                        | '╯'
                        | '┌'
                        | '┐'
                        | '└'
                        | '┘'
                        | '│'
                        | '┃'
                        | '║'
                        | '╎'
                        | '┆'
                        | '>'
                        | '_'
                        | '-'
                        | ' '
                        | '·'
                        | '•'
                        | '✻'
                        | '✽'
                        | '✶'
                        | '⠋'
                        | '⠙'
                        | '⠹'
                        | '⠸'
                        | '⠼'
                        | '⠴'
                        | '⠦'
                        | '⠧'
                        | '⠇'
                        | '⠏'
                        | '❯'
                        | '�'
                        | '▐'
                        | '▛'
                        | '█'
                        | '▜'
                        | '▌'
                        | '▝'
                        | '▘'
                )
        })
}

fn strip_ansi_and_control(input: &str) -> String {
    let mut output = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for seq_ch in chars.by_ref() {
                        if ('@'..='~').contains(&seq_ch) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    let mut prev_escape = false;
                    for seq_ch in chars.by_ref() {
                        if seq_ch == '\u{7}' || (prev_escape && seq_ch == '\\') {
                            break;
                        }
                        prev_escape = seq_ch == '\x1b';
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
        } else if ch == '\r' {
            output.push('\n');
        } else if ch == '\n' || ch == '\t' || !ch.is_control() {
            output.push(ch);
        }
    }
    output
}

fn thread_runtime_response(thread: &ClaudeThread, include_turns: bool) -> Value {
    json!({
        "thread": thread.to_json(include_turns),
        "model": thread.model,
        "modelProvider": PROVIDER_NAME,
        "serviceTier": Value::Null,
        "cwd": thread.cwd,
        "runtimeWorkspaceRoots": [],
        "instructionSources": [],
        "approvalPolicy": "on-request",
        "approvalsReviewer": "user",
        "sandbox": { "type": "dangerFullAccess" },
        "activePermissionProfile": Value::Null,
        "reasoningEffort": Value::Null,
    })
}

fn config_read_response(params: &Value) -> Value {
    let layers = params
        .get("includeLayers")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        .then(|| json!([]))
        .unwrap_or(Value::Null);
    json!({
        "config": {
            "model": Value::Null,
            "review_model": Value::Null,
            "model_context_window": Value::Null,
            "model_auto_compact_token_limit": Value::Null,
            "model_auto_compact_token_limit_scope": Value::Null,
            "model_provider": Value::Null,
            "approval_policy": Value::Null,
            "approvals_reviewer": Value::Null,
            "sandbox_mode": Value::Null,
            "sandbox_workspace_write": Value::Null,
            "forced_chatgpt_workspace_id": Value::Null,
            "forced_login_method": Value::Null,
            "web_search": Value::Null,
            "tools": Value::Null,
            "instructions": Value::Null,
            "developer_instructions": Value::Null,
            "compact_prompt": Value::Null,
            "model_reasoning_effort": Value::Null,
            "model_reasoning_summary": Value::Null,
            "model_verbosity": Value::Null,
            "service_tier": Value::Null,
            "analytics": Value::Null,
            "desktop": Value::Null,
        },
        "origins": {},
        "layers": layers,
    })
}

fn model_from_params(params: &Value) -> String {
    params
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            std::env::var(MODEL_ENV)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_MODEL.to_string())
}

fn prompt_from_input(input: &[Value]) -> String {
    let mut parts = Vec::new();
    for item in input {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            }
            Some("localImage") => {
                if let Some(path) = item.get("path").and_then(Value::as_str) {
                    parts.push(format!("[local image: {}]", path));
                }
            }
            Some("image") => {
                if let Some(url) = item.get("url").and_then(Value::as_str) {
                    parts.push(format!("[image: {}]", url));
                }
            }
            Some("mention") | Some("skill") => {
                if let Some(name) = item.get("name").and_then(Value::as_str) {
                    parts.push(format!("@{}", name));
                }
            }
            _ => {}
        }
    }
    let prompt = parts.join("\n\n").trim().to_string();
    if prompt.is_empty() {
        "(empty prompt)".to_string()
    } else {
        prompt
    }
}

fn normalize_cwd(value: Option<&str>) -> String {
    let path = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let absolute = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    absolute.to_string_lossy().to_string()
}

fn required_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing required param: {}", key))
}

fn uuid_from_thread_id(value: &str) -> String {
    if is_uuid_like(value) {
        value.to_string()
    } else {
        new_uuid_v4()
    }
}

fn is_uuid_like(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23].iter().all(|index| bytes[*index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
}

fn new_uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    for byte in &mut bytes {
        *byte = rand::random::<u8>();
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

fn trim_json_line(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n")
        .unwrap_or(line)
        .strip_suffix(b"\r")
        .unwrap_or_else(|| line.strip_suffix(b"\n").unwrap_or(line))
}

fn lock_state(
    state: &SharedState,
) -> Result<std::sync::MutexGuard<'_, ClaudeAppServerState>, String> {
    state
        .lock()
        .map_err(|_| "claude-code app-server state mutex poisoned".to_string())
}

fn write_response<W: Write>(
    output: &SharedOutput<W>,
    id: Value,
    result: Value,
) -> Result<(), String> {
    write_json_line(output, &json!({ "id": id, "result": result }))
}

fn write_error<W: Write>(
    output: &SharedOutput<W>,
    id: Value,
    code: i64,
    message: String,
) -> Result<(), String> {
    write_json_line(
        output,
        &json!({
            "id": id,
            "error": {
                "code": code,
                "message": message,
            },
        }),
    )
}

fn write_notification<W: Write>(
    output: &SharedOutput<W>,
    notification: Value,
) -> Result<(), String> {
    write_json_line(output, &notification)
}

fn write_json_line<W: Write>(output: &SharedOutput<W>, value: &Value) -> Result<(), String> {
    let mut line = serde_json::to_vec(value).map_err(|err| err.to_string())?;
    line.push(b'\n');
    let mut output = output
        .lock()
        .map_err(|_| "claude-code app-server stdout mutex poisoned".to_string())?;
    output
        .write_all(&line)
        .map_err(|err| format!("failed to write app-server stdout: {}", err))?;
    output
        .flush()
        .map_err(|err| format!("failed to flush app-server stdout: {}", err))
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn elapsed_millis(started: Instant) -> i64 {
    started.elapsed().as_millis() as i64
}

fn non_empty_string(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn non_empty_join(parts: &[String], separator: &str) -> String {
    let joined = parts
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(separator);
    if joined.is_empty() {
        "Claude Code failed".to_string()
    } else {
        joined
    }
}

#[cfg(unix)]
fn terminate_process_group(pid: u32) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(format!("-{}", pid))
        .status();
}

#[cfg(not(unix))]
fn terminate_process_group(pid: u32) {
    let _ = Command::new("taskkill")
        .arg("/PID")
        .arg(pid.to_string())
        .args(["/T", "/F"])
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::sync::Mutex;

    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "codexl-claude-code-{}-{}-{}",
            name,
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn initialize_response_includes_codex_app_required_fields() {
        let root = test_dir("initialize");
        std::fs::create_dir_all(&root).expect("create temp dir");
        let output_path = root.join("out.jsonl");
        let input = b"{\"id\":\"1\",\"method\":\"initialize\",\"params\":{}}\n{\"method\":\"initialized\"}\n{\"id\":\"2\",\"method\":\"config/read\",\"params\":{\"includeLayers\":true}}\n";

        run_stdio_app_server_with_io(
            vec![],
            std::io::Cursor::new(input),
            File::create(&output_path).expect("create output"),
        )
        .expect("run app server");

        let output = std::fs::read_to_string(&output_path).expect("read output");
        let first_line = output.lines().next().expect("initialize response");
        let response: Value = serde_json::from_str(first_line).expect("json response");
        let result = response.get("result").expect("response result");
        assert_eq!(
            result.get("userAgent").and_then(Value::as_str).is_some(),
            true
        );
        assert_eq!(
            result.get("codexHome").and_then(Value::as_str).is_some(),
            true
        );
        assert_eq!(
            result.get("platformFamily").and_then(Value::as_str),
            Some(std::env::consts::FAMILY)
        );
        assert_eq!(
            result.get("platformOs").and_then(Value::as_str),
            Some(std::env::consts::OS)
        );
        let second_line = output.lines().nth(1).expect("config/read response");
        let config_response: Value = serde_json::from_str(second_line).expect("json response");
        let config_result = config_response.get("result").expect("config/read result");
        assert!(config_result
            .get("config")
            .and_then(Value::as_object)
            .is_some());
        assert_eq!(config_result.get("layers"), Some(&json!([])));
        assert_eq!(output.lines().count(), 2);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn filters_claude_code_tui_noise_from_streamed_and_final_text() {
        let prompt = "该项目都有哪些功能";
        let raw = r#"
▐▛███▜▌ClaudeCodev2.1.149
▝▜█████▛▘Opus4.7(1Mcontext)withmediumeffort·APIUsageBilling
▘▘▝▝~/baishan/llm-spec
◐medium·/effort
❯ 该项目都有哪些功能
✽ Brewing…
󰉋 llm-spec  feat/sdk-tester 󰚩 glm-5 ↑0 ↓0 ⚡ 0 tok/s
i…
✻n
25
✻50
✶Brewing…663
Brewing…101 tokens)
↑4
/private/var/folders/9r/example/T/codexl-claude-code-real-ccr-1
project,orworkfromyourteam).Ifnot,takeamomenttoreviewwhat'sinthisfolderfirst.
ClaudeCode'llbeabletoread,edit,andexecutefileshere.
Securityguid
Read 1 file, listed 1 directory (ctrl+o to expand)
⏺让我先了解一下项目结构和功能。
⏺ Reading 1 file, listing 1 directory… (ctrl+o to expand)
⎿ $ cat /Users/jinhuilee/baishan/llm-spec/README.md 2>/dev/null || echo "No README found"
⏺根据README和项目结构，该项目是一个LLM SDK API 兼容性测试工具，主要功能如下：
核心功能
1.多 SDK API 测试—验证三类SDK的API格式/参数/特性支持情况：
-GoogleGemini(@google/genai)
✻Baked for 22s
Resume this session with:
claude --resume acd0d82c-f9f7-4455-95fd-4ab7e0e9130b
"#;

        let streamed = clean_command_output_delta(raw, prompt);
        let final_text = clean_interactive_cli_output(raw, prompt);

        for cleaned in [&streamed, &final_text] {
            assert!(!cleaned.contains("ClaudeCodev"), "{cleaned}");
            assert!(!cleaned.contains("Opus4"), "{cleaned}");
            assert!(!cleaned.contains("Brewing"), "{cleaned}");
            assert!(!cleaned.contains("tok/s"), "{cleaned}");
            assert!(!cleaned.contains("󰉋"), "{cleaned}");
            assert!(!cleaned.contains("claude --resume"), "{cleaned}");
            assert!(!cleaned.contains("project,orworkfromyourteam"), "{cleaned}");
            assert!(!cleaned.contains("ClaudeCode'llbeabletoread"), "{cleaned}");
            assert!(
                !cleaned.contains("codexl-claude-code-real-ccr"),
                "{cleaned}"
            );
            assert!(!cleaned.contains(prompt), "{cleaned}");
            assert!(
                cleaned.contains("让我先了解一下项目结构和功能"),
                "{cleaned}"
            );
            assert!(cleaned.contains("Reading 1 file"), "{cleaned}");
            assert!(
                cleaned.contains("cat /Users/jinhuilee/baishan/llm-spec/README.md"),
                "{cleaned}"
            );
            assert!(cleaned.contains("核心功能"), "{cleaned}");
            assert!(
                cleaned.contains("-GoogleGemini(@google/genai)"),
                "{cleaned}"
            );
        }
    }

    #[test]
    fn extracts_latest_assistant_text_from_claude_transcript() {
        let transcript = r#"
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"old"}]}}
{"type":"system","message":{"role":"system","content":"ignored"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash"},{"type":"text","text":"CODEXL_CCR_APP_SERVER_OK"}]}}
"#;

        assert_eq!(
            latest_assistant_text_from_transcript(transcript),
            Some("CODEXL_CCR_APP_SERVER_OK".to_string())
        );
    }

    #[test]
    fn thread_list_read_and_turns_list_load_claude_transcripts() {
        let _env_lock = ENV_TEST_LOCK.lock().expect("env test lock");
        let old_home = std::env::var_os("HOME");
        let root = test_dir("claude-transcripts");
        let cwd = root.join("workspace");
        let session_id = "11111111-1111-4111-8111-222222222222";
        let projects_dir = root
            .join(".claude")
            .join("projects")
            .join(claude_project_dir_name(&cwd));
        std::fs::create_dir_all(&projects_dir).expect("create claude projects dir");
        std::fs::create_dir_all(&cwd).expect("create cwd");
        let transcript_path = projects_dir.join(format!("{session_id}.jsonl"));
        std::fs::write(
            &transcript_path,
            format!(
                "{}\n{}\n",
                json!({
                    "type": "user",
                    "sessionId": session_id,
                    "cwd": cwd.to_string_lossy(),
                    "timestamp": "2026-05-25T07:00:00.000Z",
                    "message": {
                        "role": "user",
                        "content": "hello transcript"
                    }
                }),
                json!({
                    "type": "assistant",
                    "sessionId": session_id,
                    "cwd": cwd.to_string_lossy(),
                    "timestamp": "2026-05-25T07:00:03.000Z",
                    "uuid": "assistant-message",
                    "message": {
                        "role": "assistant",
                        "model": "opus",
                        "content": [{ "type": "text", "text": "hello from claude" }]
                    }
                })
            ),
        )
        .expect("write transcript");
        std::env::set_var("HOME", &root);

        let output_path = root.join("out.jsonl");
        let input = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            json!({"id":"1","method":"initialize","params":{}}),
            json!({"id":"2","method":"thread/list","params":{"limit":10,"sourceKinds":["cli"],"modelProviders":[PROVIDER_NAME]}}),
            json!({"id":"3","method":"thread/read","params":{"threadId":session_id,"includeTurns":true}}),
            json!({"id":"4","method":"thread/turns/list","params":{"threadId":session_id,"limit":1,"sortDirection":"desc"}}),
            json!({"id":"5","method":"thread/resume","params":{"threadId":session_id}}),
            json!({"id":"6","method":"thread/resume","params":{"threadId":session_id,"excludeTurns":true}}),
            json!({"id":"7","method":"thread/read","params":{"threadId":format!("local:{session_id}"),"includeTurns":true}})
        );

        run_stdio_app_server_with_io(
            vec![],
            std::io::Cursor::new(input.into_bytes()),
            File::create(&output_path).expect("create output"),
        )
        .expect("run app server");

        let responses = json_lines(&std::fs::read_to_string(&output_path).expect("read output"));
        let listed = response_by_id(&responses, "2")
            .pointer("/result/data/0")
            .expect("listed thread");
        assert_eq!(listed.get("id").and_then(Value::as_str), Some(session_id));
        assert_eq!(
            listed.get("preview").and_then(Value::as_str),
            Some("hello transcript")
        );
        assert_eq!(
            listed.get("path").and_then(Value::as_str),
            Some(transcript_path.to_string_lossy().as_ref())
        );

        let read = response_by_id(&responses, "3")
            .pointer("/result/thread/turns/0")
            .expect("read turn");
        assert_eq!(
            read.pointer("/items/0/content/0/text")
                .and_then(Value::as_str),
            Some("hello transcript")
        );
        assert_eq!(
            read.pointer("/items/1/text").and_then(Value::as_str),
            Some("hello from claude")
        );

        let turn = response_by_id(&responses, "4")
            .pointer("/result/data/0")
            .expect("listed turn");
        assert_eq!(
            turn.pointer("/items/1/text").and_then(Value::as_str),
            Some("hello from claude")
        );

        let resumed = response_by_id(&responses, "5")
            .pointer("/result/thread/turns/0")
            .expect("resume returns turns by default");
        assert_eq!(
            resumed.pointer("/items/1/text").and_then(Value::as_str),
            Some("hello from claude")
        );

        let cheap_resume_turns = response_by_id(&responses, "6")
            .pointer("/result/thread/turns")
            .and_then(Value::as_array)
            .expect("cheap resume turns");
        assert!(
            cheap_resume_turns.is_empty(),
            "excludeTurns=true should omit turns"
        );

        let local_read = response_by_id(&responses, "7")
            .pointer("/result/thread/turns/0")
            .expect("local-prefixed read turn");
        assert_eq!(
            local_read.pointer("/items/1/text").and_then(Value::as_str),
            Some("hello from claude")
        );

        restore_env("HOME", old_home);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resume_unknown_thread_does_not_create_empty_claude_session() {
        let mut state = ClaudeAppServerState {
            active_processes: BTreeMap::new(),
            interrupted_turns: BTreeSet::new(),
            threads: BTreeMap::new(),
            workspace_name: None,
        };

        let err = state
            .resume_thread(&json!({
                "threadId": "22222222-2222-4222-8222-222222222222",
                "cwd": "/tmp",
            }))
            .expect_err("unknown resume should fail");

        assert!(err.contains("thread not found"), "{err}");
        assert!(state.threads.is_empty());
    }

    #[test]
    fn resume_ignores_non_claude_rollout_path() {
        let _env_lock = ENV_TEST_LOCK.lock().expect("env test lock");
        let old_home = std::env::var_os("HOME");
        let root = test_dir("non-claude-rollout");
        let cwd = root.join("workspace");
        let rollout_dir = root.join(".codex").join("sessions");
        let thread_id = "33333333-3333-4333-8333-333333333333";
        std::fs::create_dir_all(&cwd).expect("create cwd");
        std::fs::create_dir_all(&rollout_dir).expect("create rollout dir");
        let rollout_path = rollout_dir.join(format!("{thread_id}.jsonl"));
        std::fs::write(
            &rollout_path,
            format!(
                "{}\n{}\n",
                json!({
                    "type": "user",
                    "sessionId": thread_id,
                    "cwd": cwd.to_string_lossy(),
                    "timestamp": "2026-05-25T07:00:00.000Z",
                    "message": { "role": "user", "content": "codex rollout prompt" }
                }),
                json!({
                    "type": "assistant",
                    "sessionId": thread_id,
                    "cwd": cwd.to_string_lossy(),
                    "timestamp": "2026-05-25T07:00:01.000Z",
                    "message": {
                        "role": "assistant",
                        "content": [{ "type": "text", "text": "codex rollout answer" }]
                    }
                })
            ),
        )
        .expect("write non-claude rollout");
        std::env::set_var("HOME", &root);

        let mut state = ClaudeAppServerState {
            active_processes: BTreeMap::new(),
            interrupted_turns: BTreeSet::new(),
            threads: BTreeMap::new(),
            workspace_name: None,
        };
        let err = state
            .resume_thread(&json!({
                "threadId": thread_id,
                "path": rollout_path,
                "cwd": cwd,
            }))
            .expect_err("non-claude path should not resume");

        assert!(err.contains("thread not found"), "{err}");
        assert!(state.threads.is_empty());
        restore_env("HOME", old_home);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn started_thread_first_turn_uses_session_id_not_resume() {
        let mut state = ClaudeAppServerState {
            active_processes: BTreeMap::new(),
            interrupted_turns: BTreeSet::new(),
            threads: BTreeMap::new(),
            workspace_name: None,
        };
        let (response, _) = state.start_thread(&json!({ "cwd": "/tmp" }));
        let thread_id = response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .expect("thread id")
            .to_string();
        let (_, _, work) = state
            .start_turn(&json!({
                "threadId": thread_id,
                "input": [{ "type": "text", "text": "hello" }],
            }))
            .expect("start turn");

        let command = claude_command_display(&work);
        assert!(command.contains("--session-id"), "{command}");
        assert!(!command.contains("--resume"), "{command}");
    }

    #[test]
    fn codex_app_model_is_not_forwarded_to_ccr_by_default() {
        let _env_lock = ENV_TEST_LOCK.lock().expect("env test lock");
        std::env::remove_var(MODEL_ENV);
        let work = TurnWork {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            agent_item_id: "agent".to_string(),
            cli_item_id: "cli".to_string(),
            claude_session_id: "11111111-1111-4111-8111-111111111111".to_string(),
            cwd: "/tmp".to_string(),
            prompt: "hello".to_string(),
            resume_existing: false,
        };

        assert_eq!(claude_model_arg(), None);
        assert!(!claude_command_display(&work).contains("--model"));
    }

    #[test]
    fn explicit_claude_code_model_env_is_forwarded_to_ccr() {
        let _env_lock = ENV_TEST_LOCK.lock().expect("env test lock");
        std::env::set_var(MODEL_ENV, "sonnet");
        let work = TurnWork {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            agent_item_id: "agent".to_string(),
            cli_item_id: "cli".to_string(),
            claude_session_id: "11111111-1111-4111-8111-111111111111".to_string(),
            cwd: "/tmp".to_string(),
            prompt: "hello".to_string(),
            resume_existing: false,
        };

        assert_eq!(claude_model_arg(), Some("sonnet".to_string()));
        assert!(claude_command_display(&work).contains("--model sonnet"));
        std::env::remove_var(MODEL_ENV);
    }

    #[test]
    fn claude_command_uses_stream_json_protocol() {
        let work = TurnWork {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            agent_item_id: "agent".to_string(),
            cli_item_id: "cli".to_string(),
            claude_session_id: "11111111-1111-4111-8111-111111111111".to_string(),
            cwd: "/tmp".to_string(),
            prompt: "hello".to_string(),
            resume_existing: false,
        };

        let command = claude_command_display(&work);
        assert!(command.contains("--output-format stream-json"));
        assert!(command.contains("--verbose"));
        assert!(command.contains("--input-format stream-json"));
        assert!(command.contains("--include-partial-messages"));
        assert!(command.contains("--session-id 11111111-1111-4111-8111-111111111111"));
    }

    #[test]
    fn claude_stream_json_input_matches_sdk_shape() {
        let work = TurnWork {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            agent_item_id: "agent".to_string(),
            cli_item_id: "cli".to_string(),
            claude_session_id: "11111111-1111-4111-8111-111111111111".to_string(),
            cwd: "/tmp".to_string(),
            prompt: "hello".to_string(),
            resume_existing: false,
        };

        let input = claude_stream_json_input(&work);
        let lines = input
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("json line"))
            .collect::<Vec<_>>();

        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0].get("type").and_then(Value::as_str),
            Some("control_request")
        );
        assert_eq!(
            lines[0]
                .get("request")
                .and_then(|request| request.get("subtype"))
                .and_then(Value::as_str),
            Some("initialize")
        );
        assert_eq!(lines[1].get("type").and_then(Value::as_str), Some("user"));
        assert_eq!(
            lines[1]
                .get("message")
                .and_then(|message| message.get("role"))
                .and_then(Value::as_str),
            Some("user")
        );
        assert_eq!(
            lines[1]
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(Value::as_array)
                .and_then(|items| items.first())
                .and_then(|item| item.get("text"))
                .and_then(Value::as_str),
            Some("hello")
        );
    }

    #[test]
    fn parses_claude_stream_json_text_tool_and_result_events() {
        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let work = TurnWork {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            agent_item_id: "agent".to_string(),
            cli_item_id: "cli".to_string(),
            claude_session_id: "11111111-1111-4111-8111-111111111111".to_string(),
            cwd: "/tmp".to_string(),
            prompt: "hello".to_string(),
            resume_existing: false,
        };
        let mut stream = ClaudeStreamState::default();
        let mut command_output = String::new();
        let messages = [
            json!({
                "type": "stream_event",
                "parent_tool_use_id": Value::Null,
                "event": {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": "Hel" }
                }
            }),
            json!({
                "type": "stream_event",
                "parent_tool_use_id": Value::Null,
                "event": {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": "lo" }
                }
            }),
            json!({
                "type": "stream_event",
                "parent_tool_use_id": Value::Null,
                "event": {
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "Read",
                        "input": { "file_path": "/tmp/README.md" }
                    }
                }
            }),
            json!({
                "type": "stream_event",
                "parent_tool_use_id": Value::Null,
                "event": {
                    "type": "content_block_delta",
                    "index": 2,
                    "delta": { "type": "thinking_delta", "thinking": "thinking" }
                }
            }),
            json!({
                "type": "stream_event",
                "parent_tool_use_id": Value::Null,
                "event": {
                    "type": "content_block_start",
                    "index": 3,
                    "content_block": {
                        "type": "tool_result",
                        "tool_use_id": "toolu_1",
                        "content": "read result"
                    }
                }
            }),
            json!({
                "type": "stream_event",
                "parent_tool_use_id": Value::Null,
                "event": {
                    "type": "content_block_delta",
                    "index": 4,
                    "delta": { "type": "text_delta", "text": "Done" }
                }
            }),
            json!({
                "type": "result",
                "is_error": false,
                "result": "Done",
                "num_turns": 1,
                "duration_ms": 42
            }),
        ];

        for message in messages {
            handle_claude_stream_message(
                &message,
                &work,
                &output,
                &mut stream,
                &mut command_output,
            );
        }
        emit_reasoning_completed_if_started(&output, &work, &stream);

        let output =
            String::from_utf8(output.lock().expect("output lock").clone()).expect("utf8 output");
        assert!(output.contains(r#""method":"item/agentMessage/delta""#));
        assert!(output.contains(r#""delta":"Done""#));
        assert!(!output.contains(r#""type":"commandExecution""#));
        assert!(!output.contains(r#""method":"item/commandExecution/outputDelta""#));
        assert!(output.contains(r#""type":"mcpToolCall""#));
        assert!(!output.contains(r#""type":"dynamicToolCall""#));
        assert!(output.contains(r#""tool":"Read""#));
        assert!(output.contains("/tmp/README.md"));
        assert!(output.contains("read result"));
        assert!(output.contains(r#""method":"item/reasoning/textDelta""#));
        assert!(output.contains(r#""delta":"Hello""#));
        let lines = json_lines(&output);
        let tool_completed_index = lines
            .iter()
            .position(|value| {
                value.get("method").and_then(Value::as_str) == Some("item/completed")
                    && value.pointer("/params/item/type").and_then(Value::as_str)
                        == Some("mcpToolCall")
            })
            .expect("tool completed notification");
        let agent_delta_index = lines
            .iter()
            .position(|value| {
                value.get("method").and_then(Value::as_str) == Some("item/agentMessage/delta")
            })
            .expect("agent delta notification");
        assert!(agent_delta_index > tool_completed_index);
        assert_eq!(stream.emitted_text, "Done");
        assert_eq!(stream.result_text, Some("Done".to_string()));
        assert_eq!(stream.completed_tool_items.len(), 1);
    }

    #[test]
    fn streamed_tool_arguments_are_visible_on_started_item() {
        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let work = TurnWork {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            agent_item_id: "agent".to_string(),
            cli_item_id: "cli".to_string(),
            claude_session_id: "11111111-1111-4111-8111-111111111111".to_string(),
            cwd: "/tmp".to_string(),
            prompt: "hello".to_string(),
            resume_existing: false,
        };
        let mut stream = ClaudeStreamState::default();
        let mut command_output = String::new();
        let messages = [
            json!({
                "type": "stream_event",
                "parent_tool_use_id": Value::Null,
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_read",
                        "name": "Read"
                    }
                }
            }),
            json!({
                "type": "stream_event",
                "parent_tool_use_id": Value::Null,
                "event": {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": "{\"file_path\":\"/tmp/README.md\"}"
                    }
                }
            }),
            json!({
                "type": "stream_event",
                "parent_tool_use_id": Value::Null,
                "event": { "type": "content_block_stop", "index": 0 }
            }),
        ];

        for message in messages {
            handle_claude_stream_message(
                &message,
                &work,
                &output,
                &mut stream,
                &mut command_output,
            );
        }

        let output =
            String::from_utf8(output.lock().expect("output lock").clone()).expect("utf8 output");
        let started = json_lines(&output)
            .into_iter()
            .find(|value| value.get("method").and_then(Value::as_str) == Some("item/started"))
            .expect("tool started notification");
        assert_eq!(
            started.pointer("/params/item/type").and_then(Value::as_str),
            Some("mcpToolCall")
        );
        assert_eq!(
            started
                .pointer("/params/item/arguments/file_path")
                .and_then(Value::as_str),
            Some("/tmp/README.md")
        );
    }

    #[test]
    fn maps_agent_tool_to_current_turn_tool_call_without_spawning_thread() {
        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let work = TurnWork {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            agent_item_id: "agent".to_string(),
            cli_item_id: "cli".to_string(),
            claude_session_id: "11111111-1111-4111-8111-111111111111".to_string(),
            cwd: "/tmp".to_string(),
            prompt: "hello".to_string(),
            resume_existing: false,
        };
        let mut stream = ClaudeStreamState::default();
        let mut command_output = String::new();
        let messages = [
            json!({
                "type": "stream_event",
                "parent_tool_use_id": Value::Null,
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_agent",
                        "name": "Agent",
                        "input": {
                            "description": "Explore repo",
                            "prompt": "Inspect the project structure",
                            "subagent_type": "Explore"
                        }
                    }
                }
            }),
            json!({
                "type": "stream_event",
                "parent_tool_use_id": Value::Null,
                "event": {
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {
                        "type": "tool_result",
                        "tool_use_id": "toolu_agent",
                        "content": "subagent done"
                    }
                }
            }),
        ];

        for message in messages {
            handle_claude_stream_message(
                &message,
                &work,
                &output,
                &mut stream,
                &mut command_output,
            );
        }

        let output =
            String::from_utf8(output.lock().expect("output lock").clone()).expect("utf8 output");
        assert!(output.contains(r#""type":"mcpToolCall""#));
        assert!(output.contains(r#""tool":"Agent""#));
        assert!(output.contains("Inspect the project structure"));
        assert!(output.contains("subagent done"));
        assert!(!output.contains(r#""type":"collabAgentToolCall""#));
        assert!(!output.contains(r#""receiverThreadIds""#));
        assert!(!output.contains(r#""tool":"spawnAgent""#));
        assert!(!output.contains(r#""type":"dynamicToolCall""#));
    }

    #[test]
    fn maps_bash_tool_to_command_execution_item() {
        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let work = TurnWork {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            agent_item_id: "agent".to_string(),
            cli_item_id: "cli".to_string(),
            claude_session_id: "11111111-1111-4111-8111-111111111111".to_string(),
            cwd: "/tmp".to_string(),
            prompt: "hello".to_string(),
            resume_existing: false,
        };
        let mut stream = ClaudeStreamState::default();
        let mut command_output = String::new();
        let messages = [
            json!({
                "type": "stream_event",
                "parent_tool_use_id": Value::Null,
                "event": {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_bash",
                        "name": "Bash",
                        "input": {
                            "command": "ls -la /tmp",
                            "description": "List temp"
                        }
                    }
                }
            }),
            json!({
                "type": "stream_event",
                "parent_tool_use_id": Value::Null,
                "event": {
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {
                        "type": "tool_result",
                        "tool_use_id": "toolu_bash",
                        "content": "total 0"
                    }
                }
            }),
        ];

        for message in messages {
            handle_claude_stream_message(
                &message,
                &work,
                &output,
                &mut stream,
                &mut command_output,
            );
        }

        let output =
            String::from_utf8(output.lock().expect("output lock").clone()).expect("utf8 output");
        assert!(output.contains(r#""type":"commandExecution""#));
        assert!(output.contains("ls -la /tmp"));
        assert!(output.contains("total 0"));
    }

    #[test]
    fn ignores_matching_final_assistant_snapshot_after_text_stream() {
        let output = Arc::new(Mutex::new(Vec::<u8>::new()));
        let work = TurnWork {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            agent_item_id: "agent".to_string(),
            cli_item_id: "cli".to_string(),
            claude_session_id: "11111111-1111-4111-8111-111111111111".to_string(),
            cwd: "/tmp".to_string(),
            prompt: "hello".to_string(),
            resume_existing: false,
        };
        let mut stream = ClaudeStreamState::default();
        let mut command_output = String::new();
        let messages = [
            json!({
                "type": "stream_event",
                "parent_tool_use_id": Value::Null,
                "event": {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": "Hello\n" }
                }
            }),
            json!({
                "type": "assistant",
                "message": {
                    "role": "assistant",
                    "content": [{ "type": "text", "text": "Hello" }]
                }
            }),
        ];

        for message in messages {
            handle_claude_stream_message(
                &message,
                &work,
                &output,
                &mut stream,
                &mut command_output,
            );
        }
        flush_pending_agent_text_as_agent(&output, &work, &mut stream);

        let output =
            String::from_utf8(output.lock().expect("output lock").clone()).expect("utf8 output");
        assert_eq!(agent_delta_text(&output), "Hello\n");
        assert_eq!(stream.emitted_text, "Hello\n");
    }

    #[test]
    fn finish_turn_skips_final_agent_completed_item_after_streaming() {
        let mut state = ClaudeAppServerState {
            active_processes: BTreeMap::new(),
            interrupted_turns: BTreeSet::new(),
            threads: BTreeMap::new(),
            workspace_name: None,
        };
        let (response, _) = state.start_thread(&json!({ "cwd": "/tmp" }));
        let thread_id = response
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .expect("thread id")
            .to_string();
        let (_, _, work) = state
            .start_turn(&json!({
                "threadId": thread_id,
                "input": [{ "type": "text", "text": "hello" }],
            }))
            .expect("start turn");

        let (item_notification, turn_notification) = state
            .finish_turn(
                &work.thread_id,
                &work.turn_id,
                ClaudeRunResult {
                    text: "Hello".to_string(),
                    error: None,
                    duration_ms: 1,
                    tool_items: Vec::new(),
                    agent_item_streamed: true,
                },
            )
            .expect("finish turn");

        assert!(item_notification.is_none());
        assert_eq!(
            turn_notification.get("method").and_then(Value::as_str),
            Some("turn/completed")
        );
        let turn_items = state
            .threads
            .get(&thread_id)
            .expect("thread")
            .turns
            .first()
            .expect("turn")
            .items_json();
        assert_eq!(
            turn_items.pointer("/1/text").and_then(Value::as_str),
            Some("Hello")
        );
    }

    #[test]
    fn resolves_hidden_native_claude_when_primary_npm_bin_is_placeholder() {
        let _env_lock = ENV_TEST_LOCK.lock().expect("env test lock");
        let old_path = std::env::var_os("PATH");
        let root = test_dir("claude-path");
        let bin_dir = root.join("bin");
        let primary_dir = root
            .join("lib")
            .join("node_modules")
            .join("@anthropic-ai")
            .join("claude-code");
        let hidden_dir = root
            .join("lib")
            .join("node_modules")
            .join("@anthropic-ai")
            .join(".claude-code-good");
        let hidden_bin = hidden_dir.join("bin").join("claude.exe");

        std::fs::create_dir_all(&bin_dir).expect("create bin dir");
        std::fs::create_dir_all(primary_dir.join("bin")).expect("create primary dir");
        std::fs::create_dir_all(hidden_bin.parent().expect("hidden bin parent"))
            .expect("create hidden dir");
        write_executable(&bin_dir.join("ccr"), b"#!/bin/sh\n");
        write_executable(
            &primary_dir.join("bin").join("claude.exe"),
            b"echo \"Error: claude native binary not installed.\" >&2\n",
        );
        let file = File::create(&hidden_bin).expect("create hidden native claude");
        file.set_len(MIN_NATIVE_CLAUDE_BYTES + 1)
            .expect("size hidden native claude");
        make_executable(&hidden_bin);
        std::env::set_var("PATH", &bin_dir);

        assert_eq!(resolve_claude_path_for_ccr("ccr"), Some(hidden_bin));

        if let Some(path) = old_path {
            std::env::set_var("PATH", path);
        } else {
            std::env::remove_var("PATH");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_codex_claude_path_override_sets_claude_path_env() {
        let _env_lock = ENV_TEST_LOCK.lock().expect("env test lock");
        let old_override = std::env::var_os(CLAUDE_PATH_OVERRIDE_ENV);
        let old_claude_path = std::env::var_os(CLAUDE_PATH_ENV);
        let override_path = "/tmp/custom-claude";
        std::env::set_var(CLAUDE_PATH_OVERRIDE_ENV, override_path);
        std::env::remove_var(CLAUDE_PATH_ENV);

        let mut command = Command::new("ccr");
        configure_claude_path_env(&mut command, "ccr");
        let claude_path = command
            .get_envs()
            .find_map(|(key, value)| {
                (key == CLAUDE_PATH_ENV).then(|| value.map(|value| value.to_os_string()))
            })
            .flatten();

        assert_eq!(claude_path, Some(OsString::from(override_path)));

        restore_env(CLAUDE_PATH_OVERRIDE_ENV, old_override);
        restore_env(CLAUDE_PATH_ENV, old_claude_path);
    }

    #[test]
    #[ignore = "requires real ccr code service and Claude Code auth"]
    fn real_ccr_code_stream_json_smoke() {
        let _env_lock = ENV_TEST_LOCK.lock().expect("env test lock");
        let root = test_dir("real-ccr");
        std::fs::create_dir_all(&root).expect("create temp dir");
        std::env::set_var(BIN_ENV, "ccr");
        std::env::set_var(BASE_ARGS_ENV, "code");
        std::env::remove_var(EXTRA_ARGS_ENV);
        std::env::remove_var(MODEL_ENV);
        std::env::remove_var(PERMISSION_MODE_ENV);

        let output_path = root.join("out.jsonl");
        let thread_id = new_uuid_v4();
        let claude_project_dir = PathBuf::from(std::env::var_os("HOME").expect("HOME set"))
            .join(".claude")
            .join("projects")
            .join(claude_project_dir_name(&root));
        std::fs::create_dir_all(&claude_project_dir).expect("create claude project dir");
        let transcript_path = claude_project_dir.join(format!("{thread_id}.jsonl"));
        std::fs::write(&transcript_path, "").expect("create empty transcript");
        let token = format!(
            "CODEXL_CCR_APP_SERVER_OK_{}",
            thread_id
                .chars()
                .filter(|ch| ch.is_ascii_hexdigit())
                .take(8)
                .collect::<String>()
        );
        let input = format!(
            "{{\"id\":\"1\",\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"2025-11-25\"}}}}\n{{\"method\":\"initialized\",\"params\":{{}}}}\n{{\"id\":\"2\",\"method\":\"thread/resume\",\"params\":{{\"threadId\":\"{}\",\"path\":\"{}\",\"cwd\":\"{}\",\"model\":\"sonnet\"}}}}\n{{\"id\":\"3\",\"method\":\"turn/start\",\"params\":{{\"threadId\":\"{}\",\"input\":[{{\"type\":\"text\",\"text\":\"Reply exactly with this token and nothing else: {}\"}}]}}}}\n",
            thread_id,
            transcript_path.to_string_lossy(),
            root.to_string_lossy(),
            thread_id,
            token
        );

        run_stdio_app_server_with_io(
            vec![],
            std::io::Cursor::new(input.into_bytes()),
            File::create(&output_path).expect("create output"),
        )
        .expect("run app server");

        std::env::remove_var(BIN_ENV);
        std::env::remove_var(BASE_ARGS_ENV);

        let output = std::fs::read_to_string(&output_path).expect("read output");
        assert!(output.contains(r#""method":"item/started""#));
        assert!(!output.contains("ccr code --output-format"));
        assert!(!output.contains(r#""type":"commandExecution""#));
        assert!(output.contains(r#""method":"item/agentMessage/delta""#));
        assert!(
            !output.contains("claude: command not found"),
            "output was:\n{}",
            output
        );
        assert!(
            !output.contains("claude native binary not installed"),
            "output was:\n{}",
            output
        );
        assert!(
            agent_delta_text(&output).contains(&token),
            "output was:\n{}",
            output
        );
        assert!(!output.contains(r#""method":"item/completed""#));
        assert!(output.contains(r#""method":"turn/completed""#));

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(claude_project_dir);
    }

    #[test]
    #[ignore = "requires real ccr code service, Claude Code auth, and tool execution"]
    fn real_ccr_code_stream_json_tool_smoke() {
        let _env_lock = ENV_TEST_LOCK.lock().expect("env test lock");
        let root = test_dir("real-ccr-tool");
        std::fs::create_dir_all(&root).expect("create temp dir");
        std::env::set_var(BIN_ENV, "ccr");
        std::env::set_var(BASE_ARGS_ENV, "code");
        std::env::remove_var(EXTRA_ARGS_ENV);
        std::env::remove_var(MODEL_ENV);
        std::env::remove_var(PERMISSION_MODE_ENV);

        let output_path = root.join("out.jsonl");
        let thread_id = new_uuid_v4();
        let claude_project_dir = PathBuf::from(std::env::var_os("HOME").expect("HOME set"))
            .join(".claude")
            .join("projects")
            .join(claude_project_dir_name(&root));
        std::fs::create_dir_all(&claude_project_dir).expect("create claude project dir");
        let transcript_path = claude_project_dir.join(format!("{thread_id}.jsonl"));
        std::fs::write(&transcript_path, "").expect("create empty transcript");
        let token = format!(
            "CODEXL_CCR_TOOL_OK_{}",
            thread_id
                .chars()
                .filter(|ch| ch.is_ascii_hexdigit())
                .take(8)
                .collect::<String>()
        );
        let marker_path = root.join("marker.txt");
        std::fs::write(&marker_path, &token).expect("write marker");
        let input = format!(
            "{{\"id\":\"1\",\"method\":\"initialize\",\"params\":{{\"protocolVersion\":\"2025-11-25\"}}}}\n{{\"method\":\"initialized\",\"params\":{{}}}}\n{{\"id\":\"2\",\"method\":\"thread/resume\",\"params\":{{\"threadId\":\"{}\",\"path\":\"{}\",\"cwd\":\"{}\",\"model\":\"sonnet\"}}}}\n{{\"id\":\"3\",\"method\":\"turn/start\",\"params\":{{\"threadId\":\"{}\",\"input\":[{{\"type\":\"text\",\"text\":\"Use the Read tool to read this file, then reply exactly with its full contents and nothing else: {}\"}}]}}}}\n",
            thread_id,
            transcript_path.to_string_lossy(),
            root.to_string_lossy(),
            thread_id,
            marker_path.to_string_lossy()
        );

        run_stdio_app_server_with_io(
            vec![],
            std::io::Cursor::new(input.into_bytes()),
            File::create(&output_path).expect("create output"),
        )
        .expect("run app server");

        std::env::remove_var(BIN_ENV);
        std::env::remove_var(BASE_ARGS_ENV);

        let output = std::fs::read_to_string(&output_path).expect("read output");
        assert!(
            !output.contains("ccr code --output-format"),
            "output was:\n{output}"
        );
        assert!(
            !output.contains(r#""type":"commandExecution""#),
            "output was:\n{output}"
        );
        assert!(
            output.contains(r#""type":"mcpToolCall""#),
            "output was:\n{output}"
        );
        assert!(
            !output.contains(r#""type":"dynamicToolCall""#),
            "output was:\n{output}"
        );
        assert!(output.contains(r#""tool":"Read""#), "output was:\n{output}");
        assert!(
            output.contains(marker_path.to_string_lossy().as_ref()),
            "output was:\n{output}"
        );
        assert!(
            agent_delta_text(&output).contains(&token),
            "output was:\n{}",
            output
        );

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(claude_project_dir);
    }

    fn agent_delta_text(output: &str) -> String {
        output
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|value| {
                value.get("method").and_then(Value::as_str) == Some("item/agentMessage/delta")
            })
            .filter_map(|value| {
                value
                    .get("params")
                    .and_then(|params| params.get("delta"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    }

    fn json_lines(output: &str) -> Vec<Value> {
        output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("json line"))
            .collect()
    }

    fn response_by_id<'a>(responses: &'a [Value], id: &str) -> &'a Value {
        responses
            .iter()
            .find(|response| response.get("id").and_then(Value::as_str) == Some(id))
            .unwrap_or_else(|| panic!("missing response id {id}: {responses:#?}"))
    }

    fn write_executable(path: &Path, contents: &[u8]) {
        std::fs::write(path, contents).expect("write executable");
        make_executable(path);
    }

    fn make_executable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(path)
                .expect("executable metadata")
                .permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(path, permissions).expect("chmod executable");
        }
        #[cfg(not(unix))]
        {
            let _ = path;
        }
    }

    fn restore_env(name: &str, value: Option<OsString>) {
        if let Some(value) = value {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }
}
