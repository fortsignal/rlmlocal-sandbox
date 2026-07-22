//! OpenCode process host — Phase 1 lifecycle for the Code door.
//!
//! Spawns/stops/health-checks `opencode serve` under our Tauri hands process.
//! Does NOT embed Electron or OpenCode UI. Binary via OPENCODE_BIN or PATH.
//!
//! **No OpenCode server password.** Localhost-only bind; product governance is ours
//! (pairing on :1421, Approve → apply_patch, FortSignal later — last, not here).
//!
//! Plan: planning/active/OPENCODE-INTEGRATION-MASTER-PLAN-2026-07-15.md

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use reqwest::blocking::Client as HttpClient;
use tiny_http::{Header, Method, Response, Server, StatusCode};

/// RLMlocal's serve port. OpenCode documents 4096, but Kilo commonly owns it
/// in this dev setup; the Tauri Code door must never compete with that tool.
pub const OPENCODE_PORT: u16 = 4097;

#[derive(serde::Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OpencodeStatus {
  pub running: bool,
  pub port: u16,
  pub pid: Option<u32>,
  pub binary: Option<String>,
  pub project_path: Option<String>,
  pub healthy: bool,
  pub last_error: Option<String>,
  pub version_hint: Option<String>,
}

struct HostState {
  child: Option<Child>,
  project_path: Option<String>,
  last_error: Option<String>,
  binary: Option<String>,
}

static HOST: Mutex<HostState> = Mutex::new(HostState {
  child: None,
  project_path: None,
  last_error: None,
  binary: None,
});

/// There is deliberately only one editable OpenCode task at a time. OpenCode's
/// serve API has one active agent loop, and a task owns both that session's
/// shadow and its baseline snapshot. The real worktree is never this path.
struct AgentTask {
  id: String,
  real_root: PathBuf,
  shadow_root: PathBuf,
  baseline: BTreeMap<String, String>,
  continued: bool,
  model: String,
  provider: TaskProvider,
}

static AGENT_TASK: Mutex<Option<AgentTask>> = Mutex::new(None);

/// Browser-to-native provider description.  The model stays selected by the
/// cockpit, but the native host owns the actual connection used by OpenCode.
/// In particular, an edge key lives only in this in-memory task, never in an
/// OpenCode config file or the project shadow.
#[derive(Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum OpenCodeProvider {
  Ollama { #[serde(default, rename = "baseURL", alias = "baseUrl")] base_url: Option<String> },
  Edge {
    #[serde(rename = "workerURL", alias = "workerUrl")]
    worker_url: String,
    #[serde(default, rename = "ownerKey", alias = "owner_key")]
    owner_key: Option<String>,
    /// Workers AI counts prompt + completion in one context window. The
    /// browser supplies the selected model's conservative usable window.
    #[serde(default, rename = "contextWindow", alias = "context_window")]
    context_window: Option<usize>,
  },
}

enum TaskProvider {
  Ollama { base_url: Option<String> },
  Edge { proxy: EdgeProxy },
}

struct EdgeProxy {
  base_url: String,
  api_key: String,
  stop: Arc<AtomicBool>,
  join: Option<JoinHandle<()>>,
}

impl EdgeProxy {
  fn base_url(&self) -> &str { &self.base_url }
  fn api_key(&self) -> &str { &self.api_key }
}

impl Drop for EdgeProxy {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Relaxed);
    if let Some(join) = self.join.take() {
      let _ = join.join();
    }
  }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpencodeTaskStart {
  pub task_id: String,
  pub shadow_path: String,
  pub port: u16,
}

/// Resolve the OpenCode CLI: env OPENCODE_BIN, then `opencode` on PATH.
pub fn resolve_binary() -> Result<PathBuf, String> {
  if let Ok(p) = std::env::var("OPENCODE_BIN") {
    let pb = PathBuf::from(p.trim());
    if pb.is_file() {
      return Ok(pb);
    }
    return Err(format!("OPENCODE_BIN set but not a file: {}", pb.display()));
  }
  let name = if cfg!(windows) { "opencode.exe" } else { "opencode" };
  if let Ok(path_var) = std::env::var("PATH") {
    for dir in std::env::split_paths(&path_var) {
      let cand = dir.join(name);
      if cand.is_file() {
        return Ok(cand);
      }
    }
  }
  Err(
    "opencode CLI not found — install it (curl -fsSL https://opencode.ai/install | bash) \
     or set OPENCODE_BIN to the binary path"
      .into(),
  )
}

fn http_get_local(port: u16, path: &str, timeout_ms: u64) -> Result<(u16, String), String> {
  let addr = format!("127.0.0.1:{port}");
  let mut stream = TcpStream::connect(&addr).map_err(|e| format!("connect {addr}: {e}"))?;
  stream
    .set_read_timeout(Some(Duration::from_millis(timeout_ms)))
    .ok();
  stream
    .set_write_timeout(Some(Duration::from_millis(timeout_ms)))
    .ok();
  let req = format!(
    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
  );
  stream
    .write_all(req.as_bytes())
    .map_err(|e| format!("write: {e}"))?;
  let mut buf = String::new();
  stream
    .read_to_string(&mut buf)
    .map_err(|e| format!("read: {e}"))?;
  let status = buf
    .lines()
    .next()
    .and_then(|l| l.split_whitespace().nth(1))
    .and_then(|s| s.parse::<u16>().ok())
    .unwrap_or(0);
  let body = if let Some(idx) = buf.find("\r\n\r\n") {
    buf[idx + 4..].to_string()
  } else {
    buf
  };
  Ok((status, body))
}

fn port_open(port: u16) -> bool {
  let addr: SocketAddr = ([127, 0, 0, 1], port).into();
  TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// Health: no password. Prefer /global/health 200; else TCP listen = up (booting).
pub fn probe_healthy(port: u16) -> bool {
  match http_get_local(port, "/global/health", 800) {
    Ok((status, body)) => {
      if status == 200 && (body.contains("healthy") || body.contains("version") || body.contains("true")) {
        return true;
      }
      // Listening but not ready / unexpected body — still "up" if TCP works
      status != 0 && port_open(port)
    }
    Err(_) => port_open(port),
  }
}

fn reap_if_exited(st: &mut HostState) {
  if let Some(child) = st.child.as_mut() {
    match child.try_wait() {
      Ok(Some(status)) => {
        st.last_error = Some(format!("opencode exited: {status}"));
        st.child = None;
      }
      Ok(None) => {}
      Err(e) => {
        st.last_error = Some(format!("try_wait: {e}"));
        st.child = None;
      }
    }
  }
}

pub fn status_inner() -> OpencodeStatus {
  let mut st = HOST.lock().unwrap_or_else(|e| e.into_inner());
  reap_if_exited(&mut st);
  let running = st.child.is_some();
  let healthy = probe_healthy(OPENCODE_PORT);
  let pid = st.child.as_ref().map(|c| c.id());
  OpencodeStatus {
    running: running || healthy,
    port: OPENCODE_PORT,
    pid,
    binary: st.binary.clone().or_else(|| resolve_binary().ok().map(|p| p.display().to_string())),
    project_path: st.project_path.clone(),
    healthy,
    last_error: st.last_error.clone(),
    version_hint: None,
  }
}

pub fn stop_inner() -> Result<OpencodeStatus, String> {
  let mut st = HOST.lock().unwrap_or_else(|e| e.into_inner());
  if let Some(mut child) = st.child.take() {
    let _ = child.kill();
    let _ = child.wait();
    st.last_error = None;
    eprintln!("[opencode] stopped");
  }
  drop(st);
  // Stop is the explicit user cancellation path in the Code dock. A stopped
  // server must not leave its one-active-task lock (or disposable worktree)
  // behind, otherwise the next Send incorrectly falls back to legacy mode.
  let cancelled = {
    let mut task = AGENT_TASK.lock().unwrap_or_else(|e| e.into_inner());
    task.take()
  };
  if let Some(task) = cancelled {
    crate::remove_shadow(&task.real_root, &task.shadow_root);
    eprintln!("[opencode] cancelled task {}", task.id);
  }
  Ok(status_inner())
}

pub fn start_inner(project_path: Option<String>) -> Result<OpencodeStatus, String> {
  start_inner_with_config(project_path, None)
}

/// Start a server we own. Task mode supplies an inline config so the agent can
/// edit its shadow but cannot reach outside the project or use arbitrary bash.
fn start_inner_with_config(
  project_path: Option<String>,
  config: Option<String>,
) -> Result<OpencodeStatus, String> {
  // Already up (us or external) — don't double-spawn
  if probe_healthy(OPENCODE_PORT) {
    let mut st = HOST.lock().unwrap_or_else(|e| e.into_inner());
    if st.child.is_none() {
      st.last_error = None;
      st.project_path = project_path.or(st.project_path.clone());
    }
    drop(st);
    return Ok(status_inner());
  }

  let bin = resolve_binary()?;
  let cwd = project_path
    .as_ref()
    .map(PathBuf::from)
    .filter(|p| p.is_dir())
    .ok_or_else(|| {
      "projectPath required and must be an existing directory (set path in exec console)".to_string()
    })?;

  {
    let mut st = HOST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut child) = st.child.take() {
      let _ = child.kill();
      let _ = child.wait();
    }
  }

  let mut cmd = Command::new(&bin);
  cmd
    .arg("serve")
    .arg("--hostname")
    .arg("127.0.0.1")
    .arg("--port")
    .arg(OPENCODE_PORT.to_string())
    .arg("--cors")
    .arg("http://localhost:5173")
    .arg("--cors")
    .arg("http://127.0.0.1:5173")
    .arg("--cors")
    .arg("https://rlmlocal.com")
    .arg("--cors")
    .arg("https://www.rlmlocal.com")
    // Explicitly no OpenCode server password — governance is ours (pair + Approve + FortSignal later)
    .env_remove("OPENCODE_SERVER_PASSWORD")
    .current_dir(&cwd)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
  if let Some(config) = config {
    cmd.env("OPENCODE_CONFIG_CONTENT", config);
  }

  let mut child = cmd
    .spawn()
    .map_err(|e| format!("failed to spawn {}: {e}", bin.display()))?;

  let deadline = Instant::now() + Duration::from_secs(8);
  let mut healthy = false;
  while Instant::now() < deadline {
    if probe_healthy(OPENCODE_PORT) {
      healthy = true;
      break;
    }
    match child.try_wait() {
      Ok(Some(status)) => {
        let mut err = format!("opencode exited early: {status}");
        if let Some(mut stderr) = child.stderr.take() {
          let mut s = String::new();
          let _ = stderr.read_to_string(&mut s);
          if !s.is_empty() {
            err.push_str(": ");
            err.push_str(s.chars().take(400).collect::<String>().as_str());
          }
        }
        let mut st = HOST.lock().unwrap_or_else(|e| e.into_inner());
        st.last_error = Some(err.clone());
        st.child = None;
        return Err(err);
      }
      _ => {}
    }
    std::thread::sleep(Duration::from_millis(200));
  }

  let pid = child.id();
  {
    let mut st = HOST.lock().unwrap_or_else(|e| e.into_inner());
    st.binary = Some(bin.display().to_string());
    st.project_path = Some(cwd.display().to_string());
    if healthy {
      st.last_error = None;
      st.child = Some(child);
      eprintln!(
        "[opencode] serve up on :{OPENCODE_PORT} pid={pid} cwd={} (no password — localhost only)",
        cwd.display()
      );
    } else {
      st.last_error =
        Some("spawned but health check timed out (8s) — check OpenCode install / port 4096".into());
      st.child = Some(child);
      eprintln!("[opencode] spawned pid={pid} but health timeout");
    }
  }

  Ok(status_inner())
}

pub fn ensure_inner(project_path: Option<String>) -> Result<OpencodeStatus, String> {
  let s = status_inner();
  if s.healthy {
    return Ok(s);
  }
  start_inner(project_path)
}

/// One-shot coding task via `opencode run` (not multi-session serve).
/// Avoids MessageAbortedError when serve only runs one agent loop at a time.
/// `model` = "provider/id" e.g. "ollama/qwen2.5-coder:7b".
/// `ollama_base` = optional OpenAI-compat base (default http://127.0.0.1:11434/v1).
pub fn run_inner(
  project_path: Option<String>,
  message: String,
  model: Option<String>,
  ollama_base: Option<String>,
  provider: Option<OpenCodeProvider>,
) -> Result<serde_json::Value, String> {
  let cwd = project_path
    .as_ref()
    .map(PathBuf::from)
    .filter(|p| p.is_dir())
    .ok_or_else(|| "projectPath required for opencode run".to_string())?;
  let msg = message.trim().to_string();
  if msg.is_empty() {
    return Err("message required".into());
  }
  let model = model
    .unwrap_or_else(|| "ollama/qwen2.5-coder:7b".into())
    .trim()
    .to_string();
  let bin = resolve_binary()?;
  let task_provider = resolve_task_provider(&model, ollama_base, provider)?;

  // Inline config keeps the project tree clean and makes the policy apply only to this run.
  let opencode_config = build_rlm_opencode_config(&model, &task_provider)?;

  // Multi-step agent brief: use tools (read/grep/glob/bash) then emit write proposals.
  // edit/write DENIED in inline config — human Approve lands in RLM (no silent disk).
  let full_msg = format!(
    "You are the GRAPH CODER implementer (OpenCode multi-step tools) for project {}.\n\
     Rules:\n\
     - IMPLEMENT the task with multi-step tools: read/grep/glob existing code BEFORE proposing writes.\n\
     - No greetings, tutorials, todowrite, or fake /path/to/… paths.\n\
     - Disk write/edit tools are blocked. After reading, emit COMPLETE write proposal JSON (not partial).\n\
     - ONE JSON object per file:\n\
       {{\"name\":\"write\",\"arguments\":{{\"filePath\":\"{}/src/example.ts\",\"content\":\"export const x = 1;\\n\"}}}}\n\
     - content = full file as JSON string (\\n escapes). Balanced braces. No mid-file truncation.\n\
     - Prefer edit-sized changes: still emit full-file content for each path you change.\n\
     - No extra deps unless the task requires them.\n\
     \n\
     TASK:\n{}",
    cwd.display(),
    cwd.display(),
    msg
  );

  // --auto: multi-step tool loop; non-denied tools auto-run. edit stays deny → proposals only.
  let mut cmd = Command::new(&bin);
  cmd
    .arg("run")
    .arg("--auto")
    .arg("-m")
    .arg(&model)
    .arg("--dir")
    .arg(&cwd)
    .arg("--format")
    .arg("default")
    .arg(&full_msg)
    .env("OPENCODE_CONFIG_CONTENT", opencode_config)
    .current_dir(&cwd)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

  eprintln!("[opencode] run -m {model} --dir {}", cwd.display());

  let mut child = cmd
    .spawn()
    .map_err(|e| format!("spawn opencode run: {e}"))?;

  // Bounded wait (3 min) — local models can be slow.
  let deadline = Instant::now() + Duration::from_secs(180);
  loop {
    match child.try_wait() {
      Ok(Some(status)) => {
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut o) = child.stdout.take() {
          let _ = o.read_to_string(&mut stdout);
        }
        if let Some(mut e) = child.stderr.take() {
          let _ = e.read_to_string(&mut stderr);
        }
        let out = stdout.trim().to_string();
        let err = stderr.trim().to_string();
        let ok = status.success();
        let mut text = if !out.is_empty() {
          out
        } else if !err.is_empty() {
          err
        } else {
          format!("(no output, exit {status})")
        };

        // Small models often emit tool-call JSON as text. Parse into *proposals* — do NOT write yet.
        // Human approval sends the proposal through the shared verify/applyPatch gate.
        let proposed = parse_emitted_write_tools(&cwd, &text);
        if !proposed.is_empty() {
          let names: Vec<&str> = proposed
            .iter()
            .filter_map(|p| p.get("relative").and_then(|r| r.as_str()))
            .collect();
          text = format!(
            "{text}\n\n[rlmlocal] proposed write(s) — Approve to land: {}",
            names.join(", ")
          );
        }

        return Ok(serde_json::json!({
          "ok": ok || !proposed.is_empty(),
          "output": text.chars().take(8000).collect::<String>(),
          "model": model,
          "code": status.code().unwrap_or(-1),
          "proposed": proposed,
        }));
      }
      Ok(None) => {
        if Instant::now() > deadline {
          let _ = child.kill();
          let _ = child.wait();
          return Err("opencode run timed out (180s)".into());
        }
        std::thread::sleep(Duration::from_millis(400));
      }
      Err(e) => return Err(format!("wait: {e}")),
    }
  }
}

/// RLM land policy + optional Ollama provider for `opencode run`.
#[derive(Clone)]
struct EdgeProxyConfig {
  worker_url: String,
  owner_key: Option<String>,
  model_id: String,
  api_key: String,
  context_window: usize,
}

/// OpenCode asks OpenAI-compatible providers for a 32k completion by default.
/// That is valid for some hosts, but Workers AI enforces a *shared* input +
/// output window. Keep this policy at the provider adapter boundary instead of
/// baking an edge-model number into the generic task loop.
const DEFAULT_EDGE_CONTEXT_WINDOW: usize = 7_968;
const EDGE_MIN_OUTPUT_TOKENS: usize = 256;
const EDGE_MAX_TASK_OUTPUT_TOKENS: usize = 8_192;
const EDGE_REQUEST_MARGIN_TOKENS: usize = 1_024;

fn edge_context_window(window: Option<usize>) -> usize {
  window.filter(|value| *value >= 2_048).unwrap_or(DEFAULT_EDGE_CONTEXT_WINDOW)
}

fn openai_requested_max_tokens(incoming: &serde_json::Value) -> usize {
  incoming.get("max_tokens")
    .or_else(|| incoming.get("max_completion_tokens"))
    .and_then(|value| value.as_u64())
    .and_then(|value| usize::try_from(value).ok())
    .filter(|value| *value > 0)
    .unwrap_or(8_192)
}

/// Conservative token accounting for an OpenAI-compatible request. We do not
/// own the model tokenizer, so estimate at two characters/token and reserve
/// template overhead. The task output cap keeps a tool turn compact; OpenCode
/// can make more turns instead of trying to emit a 32k response at once.
fn edge_completion_budget(
  request_chars: usize,
  requested_max_tokens: usize,
  context_window: usize,
) -> Result<usize, String> {
  let window = edge_context_window(Some(context_window));
  let task_cap = (window / 4)
    .max(EDGE_MIN_OUTPUT_TOKENS)
    .min(EDGE_MAX_TASK_OUTPUT_TOKENS);
  let estimated_input = (request_chars.saturating_add(1) / 2)
    .saturating_add(EDGE_REQUEST_MARGIN_TOKENS);
  let available = window.saturating_sub(estimated_input);
  if available < EDGE_MIN_OUTPUT_TOKENS {
    return Err(format!(
      "edge task request is too large for its {window}-token shared context window before a completion can be reserved"
    ));
  }
  Ok(requested_max_tokens.min(task_cap).min(available))
}

fn edge_proxy_token() -> Result<String, String> {
  let mut bytes = [0u8; 24];
  getrandom::getrandom(&mut bytes).map_err(|e| format!("edge proxy token: {e}"))?;
  Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn edge_infer_url(worker_url: &str) -> Result<String, String> {
  let base = worker_url.trim().trim_end_matches('/');
  if !(base.starts_with("https://") || base.starts_with("http://")) {
    return Err("edge worker URL must start with http:// or https://".into());
  }
  Ok(if base.ends_with("/infer") { base.to_string() } else { format!("{base}/infer") })
}

// The Qwen chat template recognizes these XML-like strings as control markup.
// In an OpenCode artifact turn they are often *source text* (for example a
// sanitizer's regex), so expose opaque placeholders to the model and restore
// the exact bytes at the adapter boundary. This proxy exists only for the
// disposable OpenCode shadow; normal chat never passes through it.
const EDGE_ARTIFACT_LITERAL_SHIELDS: [(&str, &str); 8] = [
  ("<think>", "RLM_ARTIFACT_LITERAL_OPEN_THINK_9A1C"),
  ("</think>", "RLM_ARTIFACT_LITERAL_CLOSE_THINK_9A1C"),
  ("<analysis>", "RLM_ARTIFACT_LITERAL_OPEN_ANALYSIS_9A1C"),
  ("</analysis>", "RLM_ARTIFACT_LITERAL_CLOSE_ANALYSIS_9A1C"),
  ("<tool_call>", "RLM_ARTIFACT_LITERAL_OPEN_TOOL_CALL_9A1C"),
  ("</tool_call>", "RLM_ARTIFACT_LITERAL_CLOSE_TOOL_CALL_9A1C"),
  ("<function_call>", "RLM_ARTIFACT_LITERAL_OPEN_FUNCTION_CALL_9A1C"),
  ("</function_call>", "RLM_ARTIFACT_LITERAL_CLOSE_FUNCTION_CALL_9A1C"),
];

const EDGE_ARTIFACT_LITERAL_INSTRUCTION: &str = "Artifact literal safety: RLM_ARTIFACT_LITERAL_* tokens represent ordinary source text, not model control markup. Preserve them exactly in source and tool arguments; they will be restored after this turn.";

fn shield_edge_artifact_literals(text: &str) -> String {
  EDGE_ARTIFACT_LITERAL_SHIELDS.iter().fold(text.to_string(), |value, (literal, shield)| {
    value.replace(literal, shield)
  })
}

fn unshield_edge_artifact_literals(text: &str) -> String {
  EDGE_ARTIFACT_LITERAL_SHIELDS.iter().fold(text.to_string(), |value, (literal, shield)| {
    value.replace(shield, literal)
  })
}

/// Transform every model-facing JSON string without changing the OpenAI schema
/// (keys are protocol fields and deliberately remain untouched).
fn map_json_string_values(value: &serde_json::Value, map: fn(&str) -> String) -> serde_json::Value {
  match value {
    serde_json::Value::String(text) => serde_json::Value::String(map(text)),
    serde_json::Value::Array(items) => serde_json::Value::Array(
      items.iter().map(|item| map_json_string_values(item, map)).collect(),
    ),
    serde_json::Value::Object(fields) => serde_json::Value::Object(
      fields.iter().map(|(key, item)| (key.clone(), map_json_string_values(item, map))).collect(),
    ),
    _ => value.clone(),
  }
}

fn edge_artifact_messages(messages: &serde_json::Value) -> serde_json::Value {
  let mut protected = match map_json_string_values(messages, shield_edge_artifact_literals) {
    serde_json::Value::Array(items) => items,
    _ => Vec::new(),
  };
  protected.insert(0, serde_json::json!({
    "role": "system",
    "content": EDGE_ARTIFACT_LITERAL_INSTRUCTION,
  }));
  serde_json::Value::Array(protected)
}

fn normalize_edge_tool_calls(raw: &serde_json::Value, artifact_literals: bool) -> Vec<serde_json::Value> {
  raw.as_array().map(|calls| calls.iter().enumerate().filter_map(|(index, call)| {
    let function = call.get("function").unwrap_or(call);
    let name = function.get("name")?.as_str()?.trim();
    if name.is_empty() { return None; }
    let arguments = match function.get("arguments") {
      Some(serde_json::Value::String(s)) => s.clone(),
      Some(value) => serde_json::to_string(value).unwrap_or_else(|_| "{}".into()),
      None => "{}".into(),
    };
    let arguments = if artifact_literals {
      unshield_edge_artifact_literals(&arguments)
    } else {
      arguments
    };
    Some(serde_json::json!({
      "index": index,
      "id": call.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).unwrap_or("call_rlm_edge"),
      "type": "function",
      "function": { "name": name, "arguments": arguments },
    }))
  }).collect()).unwrap_or_default()
}

fn edge_openai_completion(worker: &serde_json::Value, model: &str, artifact_literals: bool) -> serde_json::Value {
  let content = worker.get("content").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
    .map(|text| if artifact_literals { unshield_edge_artifact_literals(text) } else { text.to_string() });
  let tool_calls = normalize_edge_tool_calls(worker.get("tool_calls").unwrap_or(&serde_json::Value::Null), artifact_literals);
  let mut message = serde_json::json!({
    "role": "assistant",
    "content": content,
  });
  if !tool_calls.is_empty() {
    message["tool_calls"] = serde_json::Value::Array(tool_calls);
  }
  serde_json::json!({
    "id": format!("chatcmpl-rlm-edge-{}", crate::now_ms()),
    "object": "chat.completion",
    "created": crate::now_ms() / 1000,
    "model": model,
    "choices": [{
      "index": 0,
      "message": message,
      "finish_reason": if worker.get("tool_calls").and_then(|v| v.as_array()).map(|v| !v.is_empty()).unwrap_or(false) { "tool_calls" } else { "stop" },
    }],
    "usage": worker.get("usage").cloned().unwrap_or_else(|| serde_json::json!({
      "prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0,
    })),
  })
}

/// Workers AI's Qwen 30B synchronous envelope can contain an empty legacy
/// `response` even while its streaming chunks contain the real OpenAI delta.
/// The browser provider already uses this route; the native OpenCode adapter
/// must normalize the same transport before returning an OpenAI completion.
fn edge_stream_text(chunk: &serde_json::Value) -> String {
  [
    chunk.pointer("/choices/0/delta/content").and_then(|value| value.as_str()),
    chunk.pointer("/choices/0/message/content").and_then(|value| value.as_str()),
    chunk.get("response").and_then(|value| value.as_str()),
    chunk.get("content").and_then(|value| value.as_str()),
    chunk.get("delta").and_then(|value| value.as_str()),
  ]
    .into_iter()
    .find(|value| value.is_some_and(|value| !value.is_empty()))
    .flatten()
    .unwrap_or_default()
    .to_string()
}

fn edge_stream_tool_calls(chunk: &serde_json::Value) -> Vec<serde_json::Value> {
  chunk.get("tool_calls").and_then(|value| value.as_array())
    .or_else(|| chunk.pointer("/choices/0/delta/tool_calls").and_then(|value| value.as_array()))
    .or_else(|| chunk.pointer("/choices/0/message/tool_calls").and_then(|value| value.as_array()))
    .cloned()
    .unwrap_or_default()
}

/// OpenAI streams tool-call arguments in fragments. Merge those fragments by
/// index, while also accepting Workers' complete top-level tool-call batches.
fn edge_stream_tool_call_arguments(raw: &serde_json::Value) -> String {
  let function = raw.get("function").unwrap_or(raw);
  match function.get("arguments") {
    Some(serde_json::Value::String(value)) => value.clone(),
    Some(value) => serde_json::to_string(value).unwrap_or_else(|_| "{}".into()),
    None => String::new(),
  }
}

fn edge_stream_tool_call_id(raw: &serde_json::Value) -> Option<&str> {
  raw.get("id").and_then(|value| value.as_str()).filter(|value| !value.is_empty())
}

fn is_complete_tool_arguments(arguments: &str) -> bool {
  matches!(
    serde_json::from_str::<serde_json::Value>(arguments.trim()),
    Ok(serde_json::Value::Object(_))
  )
}

fn next_edge_stream_tool_index(calls: &BTreeMap<usize, serde_json::Value>) -> usize {
  calls.last_key_value().map(|(index, _)| index.saturating_add(1)).unwrap_or(0)
}

/// Workers AI may stream distinct complete calls as separate top-level events
/// without an OpenAI `index`. Do not concatenate their JSON arguments into one
/// invalid call; only merge a repeated slot while its arguments are incomplete.
fn edge_stream_tool_call_index(
  calls: &BTreeMap<usize, serde_json::Value>,
  position: usize,
  raw: &serde_json::Value,
) -> usize {
  let raw_id = edge_stream_tool_call_id(raw);
  if let Some(id) = raw_id {
    if let Some((index, _)) = calls.iter().find(|(_, entry)| {
      entry.get("id").and_then(|value| value.as_str()) == Some(id)
    }) {
      return *index;
    }
  }

  let requested = raw.get("index").and_then(|value| value.as_u64())
    .and_then(|value| usize::try_from(value).ok())
    .unwrap_or(position);
  let Some(existing) = calls.get(&requested) else {
    return requested;
  };

  let existing_id = existing.get("id").and_then(|value| value.as_str());
  let previous = existing.pointer("/function/arguments")
    .and_then(|value| value.as_str())
    .unwrap_or_default();
  let incoming = edge_stream_tool_call_arguments(raw);
  let is_distinct_call = raw_id.is_some_and(|id| existing_id != Some(id))
    || (is_complete_tool_arguments(previous)
      && is_complete_tool_arguments(&incoming)
      && previous != incoming);
  if is_distinct_call {
    next_edge_stream_tool_index(calls)
  } else {
    requested
  }
}

fn merge_edge_stream_tool_call(calls: &mut BTreeMap<usize, serde_json::Value>, index: usize, raw: &serde_json::Value) {
  let function = raw.get("function").unwrap_or(raw);
  let name = function.get("name").and_then(|value| value.as_str()).unwrap_or_default();
  let arguments = edge_stream_tool_call_arguments(raw);
  let entry = calls.entry(index).or_insert_with(|| serde_json::json!({
    "index": index,
    "id": edge_stream_tool_call_id(raw).map(str::to_owned)
      .unwrap_or_else(|| format!("call_rlm_edge_{index}")),
    "type": "function",
    "function": { "name": "", "arguments": "" },
  }));
  if let Some(id) = edge_stream_tool_call_id(raw) {
    entry["id"] = serde_json::json!(id);
  }
  if !name.is_empty() {
    entry["function"]["name"] = serde_json::json!(name);
  }
  let previous = entry.pointer("/function/arguments").and_then(|value| value.as_str()).unwrap_or_default();
  let merged = if arguments.is_empty() || arguments == previous {
    previous.to_string()
  } else if arguments.starts_with(previous) {
    arguments
  } else if previous.ends_with(&arguments) {
    previous.to_string()
  } else {
    format!("{previous}{arguments}")
  };
  entry["function"]["arguments"] = serde_json::json!(merged);
}

fn edge_stream_completion(payload: &str) -> Result<serde_json::Value, String> {
  let mut content = String::new();
  let mut calls = BTreeMap::new();
  let mut saw_event = false;
  for raw_line in payload.lines() {
    let line = raw_line.trim();
    if line.is_empty() || line == "data: [DONE]" || line == "[DONE]" { continue; }
    let data = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
    if data.is_empty() { continue; }
    let chunk: serde_json::Value = serde_json::from_str(data)
      .map_err(|error| format!("invalid Workers AI stream chunk: {error}"))?;
    saw_event = true;
    if let Some(error) = chunk.get("error") {
      let detail = error.get("message").and_then(|value| value.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string());
      return Err(detail);
    }
    content.push_str(&edge_stream_text(&chunk));
    for (position, call) in edge_stream_tool_calls(&chunk).iter().enumerate() {
      let index = edge_stream_tool_call_index(&calls, position, call);
      merge_edge_stream_tool_call(&mut calls, index, call);
    }
  }
  if !saw_event {
    return Err("Workers AI stream contained no JSON events".into());
  }
  Ok(serde_json::json!({
    "content": content,
    "tool_calls": calls.into_values().collect::<Vec<_>>(),
  }))
}

fn edge_openai_sse(completion: &serde_json::Value) -> String {
  let id = completion.get("id").cloned().unwrap_or_else(|| serde_json::json!("chatcmpl-rlm-edge"));
  let model = completion.get("model").cloned().unwrap_or_else(|| serde_json::json!("rlm-edge"));
  let created = completion.get("created").cloned().unwrap_or_else(|| serde_json::json!(0));
  let message = completion.pointer("/choices/0/message").cloned().unwrap_or_else(|| serde_json::json!({}));
  let finish = completion.pointer("/choices/0/finish_reason").cloned().unwrap_or_else(|| serde_json::json!("stop"));
  let mut delta = serde_json::json!({ "role": "assistant" });
  if let Some(content) = message.get("content") {
    delta["content"] = content.clone();
  }
  if let Some(calls) = message.get("tool_calls") {
    delta["tool_calls"] = calls.clone();
  }
  let first = serde_json::json!({
    "id": id,
    "object": "chat.completion.chunk",
    "created": created,
    "model": model,
    "choices": [{ "index": 0, "delta": delta, "finish_reason": serde_json::Value::Null }],
  });
  let last = serde_json::json!({
    "id": completion.get("id").cloned().unwrap_or_else(|| serde_json::json!("chatcmpl-rlm-edge")),
    "object": "chat.completion.chunk",
    "created": completion.get("created").cloned().unwrap_or_else(|| serde_json::json!(0)),
    "model": completion.get("model").cloned().unwrap_or_else(|| serde_json::json!("rlm-edge")),
    "choices": [{ "index": 0, "delta": {}, "finish_reason": finish }],
  });
  format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n", first, last)
}

fn respond_edge_json(request: tiny_http::Request, status: u16, value: &serde_json::Value, content_type: &str) {
  let payload = serde_json::to_string(value).unwrap_or_else(|_| "{\"error\":\"edge proxy encode failure\"}".into());
  let response = Response::from_string(payload)
    .with_status_code(StatusCode(status))
    .with_header(Header::from_bytes("Content-Type", content_type).expect("valid content type"));
  let _ = request.respond(response);
}

fn respond_edge_sse(request: tiny_http::Request, payload: String) {
  let response = Response::from_string(payload)
    .with_status_code(StatusCode(200))
    .with_header(Header::from_bytes("Content-Type", "text/event-stream").expect("valid content type"))
    .with_header(Header::from_bytes("Cache-Control", "no-cache").expect("valid cache control"));
  let _ = request.respond(response);
}

fn edge_proxy_authorized(request: &tiny_http::Request, api_key: &str) -> bool {
  let expected = format!("Bearer {api_key}");
  request.headers().iter().any(|h| h.field.equiv("Authorization") && h.value.as_str() == expected)
}

fn handle_edge_proxy_request(mut request: tiny_http::Request, client: &HttpClient, cfg: &EdgeProxyConfig) {
  let path = request.url().split('?').next().unwrap_or("");
  if !edge_proxy_authorized(&request, &cfg.api_key) {
    respond_edge_json(request, 401, &serde_json::json!({ "error": { "message": "unauthorized task proxy" } }), "application/json");
    return;
  }
  if request.method() == &Method::Get && (path == "/v1/models" || path == "/models") {
    respond_edge_json(request, 200, &serde_json::json!({
      "object": "list",
      "data": [{ "id": cfg.model_id, "object": "model", "owned_by": "rlmlocal-edge" }],
    }), "application/json");
    return;
  }
  if request.method() != &Method::Post || (path != "/v1/chat/completions" && path != "/chat/completions") {
    respond_edge_json(request, 404, &serde_json::json!({ "error": { "message": "unsupported edge proxy route" } }), "application/json");
    return;
  }

  let mut body_text = String::new();
  if let Err(error) = request.as_reader().read_to_string(&mut body_text) {
    respond_edge_json(request, 400, &serde_json::json!({ "error": { "message": format!("read request: {error}") } }), "application/json");
    return;
  }
  let incoming: serde_json::Value = match serde_json::from_str(&body_text) {
    Ok(value) => value,
    Err(error) => {
      respond_edge_json(request, 400, &serde_json::json!({ "error": { "message": format!("invalid OpenAI request: {error}") } }), "application/json");
      return;
    }
  };
  let wants_stream = incoming.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
  // This localhost proxy is created only by the native OpenCode task path;
  // every request is an artifact turn, never the ordinary main-chat surface.
  let artifact_literals = true;
  let max_tokens = match edge_completion_budget(
    body_text.chars().count(),
    openai_requested_max_tokens(&incoming),
    cfg.context_window,
  ) {
    Ok(value) => value,
    Err(error) => {
      respond_edge_json(request, 400, &serde_json::json!({ "error": { "message": error } }), "application/json");
      return;
    }
  };
  let mut worker_body = serde_json::json!({
    "model": cfg.model_id,
    "messages": edge_artifact_messages(incoming.get("messages").unwrap_or(&serde_json::Value::Null)),
    "max_tokens": max_tokens,
    // Qwen 30B exposes reliable visible content through Workers AI's stream;
    // collect that stream below, then return whichever OpenAI shape OpenCode
    // requested. This mirrors CloudflareAIProvider rather than tying behavior
    // to a particular model name or OpenCode output format.
    "stream": true,
    // OpenCode owns the tool loop. Disable provider thinking markup so it can
    // never leak into tool arguments or a source edit.
    "output_mode": "artifact",
    "chat_template_kwargs": { "enable_thinking": false },
  });
  for key in ["tools", "temperature", "top_p", "stop"] {
    if let Some(value) = incoming.get(key) {
      worker_body[key] = if key == "tools" && artifact_literals {
        map_json_string_values(value, shield_edge_artifact_literals)
      } else {
        value.clone()
      };
    }
  }
  if let Some(owner_key) = cfg.owner_key.as_deref().filter(|key| !key.trim().is_empty()) {
    worker_body["ownerKey"] = serde_json::json!(owner_key);
  }

  let infer = match edge_infer_url(&cfg.worker_url) {
    Ok(url) => url,
    Err(error) => {
      respond_edge_json(request, 500, &serde_json::json!({ "error": { "message": error } }), "application/json");
      return;
    }
  };
  let mut upstream = client.post(infer).header("Content-Type", "application/json");
  if let Some(owner_key) = cfg.owner_key.as_deref().filter(|key| !key.trim().is_empty()) {
    upstream = upstream.header("X-RLM-Owner-Key", owner_key);
  }
  let upstream = match upstream.json(&worker_body).send() {
    Ok(response) => response,
    Err(error) => {
      respond_edge_json(request, 502, &serde_json::json!({ "error": { "message": format!("edge inference request failed: {error}") } }), "application/json");
      return;
    }
  };
  let status = upstream.status().as_u16();
  let payload = upstream.text().unwrap_or_default();
  let worker = match edge_stream_completion(&payload) {
    Ok(worker) => worker,
    Err(error) => {
      respond_edge_json(request, status.max(400), &serde_json::json!({ "error": { "message": error } }), "application/json");
      return;
    }
  };
  if status >= 400 || worker.get("error").is_some() {
    let message = worker.get("error")
      .and_then(|error| error.get("message").and_then(|value| value.as_str()).or_else(|| error.as_str()))
      .unwrap_or("edge inference failed")
      .to_string();
    respond_edge_json(request, status.max(400), &serde_json::json!({ "error": { "message": message } }), "application/json");
    return;
  }
  let completion = edge_openai_completion(&worker, &cfg.model_id, artifact_literals);
  if wants_stream {
    respond_edge_sse(request, edge_openai_sse(&completion));
  } else {
    respond_edge_json(request, 200, &completion, "application/json");
  }
}

/// Owner free-tier bypass for Workers `/infer`. Client may omit the key;
/// fall back to env / ~/.config so Tauri OpenCode dogfood works without relying
/// only on webview localStorage.
/// Order: client → RLM_OWNER_INFER_KEY / OWNER_INFER_KEY → ~/.config/rlmlocal/owner-infer-key
fn resolve_owner_infer_key(client: Option<String>) -> Option<String> {
  if let Some(key) = client.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
    return Some(key);
  }
  for name in ["RLM_OWNER_INFER_KEY", "OWNER_INFER_KEY"] {
    if let Ok(key) = std::env::var(name) {
      let key = key.trim().to_string();
      if !key.is_empty() {
        return Some(key);
      }
    }
  }
  if let Some(home) = std::env::var_os("HOME") {
    let path = PathBuf::from(home).join(".config/rlmlocal/owner-infer-key");
    if let Ok(raw) = std::fs::read_to_string(&path) {
      let key = raw.trim().to_string();
      if !key.is_empty() {
        return Some(key);
      }
    }
  }
  None
}

fn start_edge_proxy(
  worker_url: String,
  owner_key: Option<String>,
  model_id: String,
  context_window: Option<usize>,
) -> Result<EdgeProxy, String> {
  edge_infer_url(&worker_url)?;
  if model_id.trim().is_empty() { return Err("edge model id required".into()); }
  let owner_key = resolve_owner_infer_key(owner_key);
  if owner_key.is_none() {
    eprintln!(
      "[opencode] edge proxy: no owner infer key — free-tier 600/day will apply \
       (set localStorage rlm-owner-infer-key, env RLM_OWNER_INFER_KEY, or ~/.config/rlmlocal/owner-infer-key)"
    );
  } else {
    eprintln!("[opencode] edge proxy: owner infer bypass active");
  }
  let api_key = edge_proxy_token()?;
  let server = Server::http(("127.0.0.1", 0)).map_err(|e| format!("start edge proxy: {e}"))?;
  let port = server.server_addr().to_ip().ok_or("edge proxy did not bind an IP socket")?.port();
  let stop = Arc::new(AtomicBool::new(false));
  let thread_stop = stop.clone();
  let cfg = EdgeProxyConfig {
    worker_url,
    owner_key,
    model_id,
    api_key: api_key.clone(),
    context_window: edge_context_window(context_window),
  };
  let join = thread::Builder::new().name("rlm-edge-opencode-proxy".into()).spawn(move || {
    let client = match HttpClient::builder().timeout(Duration::from_secs(180)).build() {
      Ok(client) => client,
      Err(error) => { eprintln!("[opencode] edge proxy client failed: {error}"); return; }
    };
    while !thread_stop.load(Ordering::Relaxed) {
      match server.recv_timeout(Duration::from_millis(100)) {
        Ok(Some(request)) => handle_edge_proxy_request(request, &client, &cfg),
        Ok(None) => {}
        Err(error) => { eprintln!("[opencode] edge proxy receive failed: {error}"); break; }
      }
    }
  }).map_err(|e| format!("spawn edge proxy: {e}"))?;
  Ok(EdgeProxy {
    base_url: format!("http://127.0.0.1:{port}/v1"),
    api_key,
    stop,
    join: Some(join),
  })
}

fn resolve_task_provider(
  model: &str,
  ollama_base: Option<String>,
  provider: Option<OpenCodeProvider>,
) -> Result<TaskProvider, String> {
  match provider {
    Some(OpenCodeProvider::Ollama { base_url }) => {
      if !model.starts_with("ollama/") { return Err("Ollama task provider requires an ollama/... model".into()); }
      Ok(TaskProvider::Ollama { base_url: base_url.or(ollama_base) })
    }
    Some(OpenCodeProvider::Edge { worker_url, owner_key, context_window }) => {
      let model_id = model.strip_prefix("rlm-edge/")
        .ok_or("edge task provider requires an rlm-edge/... model")?
        .to_string();
      Ok(TaskProvider::Edge { proxy: start_edge_proxy(worker_url, owner_key, model_id, context_window)? })
    }
    None if model.starts_with("ollama/") => Ok(TaskProvider::Ollama { base_url: ollama_base }),
    None => Err("no native OpenCode provider adapter was supplied for this model".into()),
  }
}

///
/// `permission.edit: deny` covers write/edit/apply_patch — OpenCode cannot land the product tree.
/// We parse write JSON from stdout → proposals → human Approve → the shared patch gate.
/// `todowrite` / `question` denied to cut plan-theater noise on small models.
fn build_rlm_opencode_config(
  model: &str,
  provider: &TaskProvider,
) -> Result<String, String> {
  // Shared permissions for every model (graph coder land policy).
  let mut cfg = serde_json::json!({
    "$schema": "https://opencode.ai/config.json",
    "model": model,
    "permission": {
      "*": "deny",
      "edit": "deny",
      "todowrite": "deny",
      "question": "deny",
      "task": "deny",
      "skill": "deny",
      "lsp": "deny",
      "read": "allow",
      "grep": "allow",
      "glob": "allow",
      "bash": "deny",
      "webfetch": "deny",
      "websearch": "deny",
      "external_directory": "deny"
    }
  });

  match provider {
    TaskProvider::Ollama { base_url } => {
      let model_id = model.strip_prefix("ollama/").ok_or("Ollama provider/model mismatch")?;
      let base = base_url.as_deref()
        .unwrap_or("http://127.0.0.1:11434/v1")
        .trim()
        .trim_end_matches('/');
      let base = if base.ends_with("/v1") { base.to_string() } else { format!("{base}/v1") };
      cfg["provider"] = serde_json::json!({
        "ollama": {
          "npm": "@ai-sdk/openai-compatible",
          "name": "Ollama (RLMlocal)",
          "options": { "baseURL": base, "apiKey": "ollama" },
          "models": { model_id: { "name": model_id } }
        }
      });
    }
    TaskProvider::Edge { proxy } => {
      let model_id = model.strip_prefix("rlm-edge/").ok_or("edge provider/model mismatch")?;
      cfg["provider"] = serde_json::json!({
        "rlm-edge": {
          "npm": "@ai-sdk/openai-compatible",
          "name": "RLMlocal Edge task bridge",
          "options": { "baseURL": proxy.base_url(), "apiKey": proxy.api_key() },
          "models": { model_id: { "name": model_id } }
        }
      });
    }
  }

  serde_json::to_string_pretty(&cfg).map_err(|e| format!("encode opencode config: {e}"))
}

/// Task-mode policy: OpenCode may make real edits, but only inside the shadow
/// owned by this task. Verification and the human gate remain outside OpenCode.
fn build_task_opencode_config(model: &str, provider: &TaskProvider) -> Result<String, String> {
  let mut cfg: serde_json::Value = serde_json::from_str(&build_rlm_opencode_config(model, provider)?)
    .map_err(|e| format!("decode task config: {e}"))?;
  cfg["permission"]["edit"] = serde_json::json!("allow");
  cfg["permission"]["bash"] = serde_json::json!({
    "git status*": "allow",
    "git diff*": "allow",
    "npm test*": "allow",
    "npm run test*": "allow",
    "npm run lint*": "allow",
    "npx tsc --noEmit*": "allow",
    "*": "deny"
  });
  // OpenCode otherwise keeps iterating until a model decides to stop. A small
  // edge coder can repeat one rejected no-op edit indefinitely, so make the
  // documented agent guard explicit: reject the repeated call and force a
  // text response after a bounded number of tool iterations.
  cfg["permission"]["doom_loop"] = serde_json::json!("deny");
  // Agent permissions take precedence over global permissions in OpenCode.
  // Pin this invocation to a named primary agent so a developer's global
  // `build`/`plan` agent cannot silently turn our editable shadow into a
  // read-only or differently-permissioned session.
  let task_permissions = cfg["permission"].clone();
  cfg["agent"] = serde_json::json!({
    "rlm-shadow": {
      "mode": "primary",
      "description": "RLMlocal disposable-worktree coding agent",
      "temperature": 0.1,
      "steps": 12,
      "permission": task_permissions
    }
  });
  cfg["default_agent"] = serde_json::json!("rlm-shadow");
  serde_json::to_string_pretty(&cfg).map_err(|e| format!("encode task config: {e}"))
}

fn project_files(root: &Path) -> Result<BTreeMap<String, String>, String> {
  let output = Command::new("git")
    .args(["-C", root.to_str().ok_or("non-utf8 project path")?, "ls-files", "-z", "--cached", "--others", "--exclude-standard"])
    .output()
    .map_err(|e| format!("git ls-files: {e}"))?;
  if !output.status.success() {
    return Err(format!("git ls-files failed: {}", String::from_utf8_lossy(&output.stderr)));
  }
  let mut files = BTreeMap::new();
  for bytes in output.stdout.split(|b| *b == 0) {
    if bytes.is_empty() { continue; }
    let rel = std::str::from_utf8(bytes).map_err(|_| "non-utf8 git path")?.to_string();
    let p = root.join(&rel);
    if p.is_file() {
      // The coding lane is text-only. Repositories commonly contain icons and
      // other binary assets; they must not prevent a source task from getting
      // its shadow. Binary changes are deliberately not collectable here.
      match std::fs::read_to_string(&p) {
        Ok(text) => { files.insert(rel, text); }
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {}
        Err(e) => return Err(format!("read {rel}: {e}")),
      }
    }
  }
  Ok(files)
}

fn task_start_inner(
  project_path: Option<String>,
  model: Option<String>,
  ollama_base: Option<String>,
  provider: Option<OpenCodeProvider>,
) -> Result<OpencodeTaskStart, String> {
  let real_root = project_path.map(PathBuf::from).filter(|p| p.is_dir())
    .ok_or_else(|| "projectPath required for OpenCode task".to_string())?;
  let model = model.unwrap_or_else(|| "ollama/qwen2.5-coder:7b".into());
  let task_guard = AGENT_TASK.lock().unwrap_or_else(|e| e.into_inner());
  if task_guard.is_some() {
    return Err("an OpenCode task is already active; approve/reject it or wait for cleanup".into());
  }
  // Do not hold the task mutex while stopping the recovery server: stop_inner
  // also clears a cancelled task. Re-acquire only to publish the new task.
  drop(task_guard);
  let shadow = crate::make_shadow(&real_root)?;
  let baseline = match project_files(&shadow) {
    Ok(v) => v,
    Err(e) => { crate::remove_shadow(&real_root, &shadow); return Err(e); }
  };
  let task_provider = match resolve_task_provider(&model, ollama_base, provider) {
    Ok(provider) => provider,
    Err(e) => { crate::remove_shadow(&real_root, &shadow); return Err(e); }
  };
  let id = format!("oc-task-{}", crate::now_ms());
  let mut task_guard = AGENT_TASK.lock().unwrap_or_else(|e| e.into_inner());
  if task_guard.is_some() {
    crate::remove_shadow(&real_root, &shadow);
    return Err("an OpenCode task started concurrently; retry Send".into());
  }
  *task_guard = Some(AgentTask {
    id: id.clone(),
    real_root,
    shadow_root: shadow.clone(),
    baseline,
    continued: false,
    model,
    provider: task_provider,
  });
  Ok(OpencodeTaskStart { task_id: id, shadow_path: shadow.display().to_string(), port: OPENCODE_PORT })
}

/// Preserve the diagnostic channel even when OpenCode prints its formatted
/// session banner on stdout. The CLI reports provider/tool failures on stderr;
/// treating a non-empty banner as the entire result hid those failures and
/// made the browser retry an already-failed task as "no parseable write".
fn task_turn_output(stdout: &[u8], stderr: &[u8]) -> String {
  let stdout = String::from_utf8_lossy(stdout).trim().to_string();
  let stderr = String::from_utf8_lossy(stderr).trim().to_string();
  match (stdout.is_empty(), stderr.is_empty()) {
    (true, true) => String::new(),
    (false, true) => stdout,
    (true, false) => stderr,
    (false, false) => format!("{stdout}\n\n[OpenCode stderr]\n{stderr}"),
  }
}

/// A graph brief is assembled in the browser, where the real project path is
/// useful to host APIs. An OpenCode task runs in a disposable shadow instead.
/// Do not leak that real absolute root to the task: OpenCode correctly treats
/// it as an external directory and refuses the read. Keep this native boundary
/// even though graph briefs are also relative-path-only, so every caller gets
/// the same protection.
fn task_message_in_shadow(message: &str, real_root: &Path) -> String {
  let root = real_root.to_string_lossy();
  let root = root.trim_end_matches(['/', '\\']);
  let relative = if root.is_empty() {
    message.to_string()
  } else {
    message.replace(root, ".")
  };
  format!(
    "{relative}\n\n## Disposable worktree tool rules\n\
You are already inside the disposable worktree. Use repository-relative paths only (for example `src/file.ts`); never use an absolute path.\n\
Call exactly one tool with exactly one JSON argument object at a time. If you need two files, call `read` separately and wait for each result before the next call.\n\
Make edits only in this worktree; the real project is intentionally inaccessible."
  )
}

/// One persistent CLI conversation in the task shadow. `opencode serve` sessions
/// can stall before dispatching local Ollama; the CLI's --continue path uses the
/// same durable session while retaining OpenCode's proven tool loop.
fn task_turn_inner(task_id: String, message: String, requested_model: Option<String>, _ollama_base: Option<String>) -> Result<String, String> {
  let mut tasks = AGENT_TASK.lock().unwrap_or_else(|e| e.into_inner());
  let task = tasks.as_mut().ok_or("no active OpenCode task")?;
  if task.id != task_id { return Err("OpenCode task id does not match active task".into()); }
  if let Some(model) = requested_model.filter(|model| !model.trim().is_empty()) {
    if model != task.model {
      return Err("OpenCode task model is pinned at start; end this task before switching models".into());
    }
  }
  let cfg = build_task_opencode_config(&task.model, &task.provider)?;
  let bin = resolve_binary()?;
  let mut cmd = Command::new(bin);
  cmd.arg("run").arg("--auto").arg("--agent").arg("rlm-shadow").arg("-m").arg(&task.model).arg("--dir").arg(&task.shadow_root).arg("--format").arg("default");
  if task.continued { cmd.arg("--continue"); }
  let message = task_message_in_shadow(&message, &task.real_root);
  cmd.arg(message).env("OPENCODE_CONFIG_CONTENT", cfg).current_dir(&task.shadow_root).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
  let out = cmd.output().map_err(|e| format!("spawn OpenCode task turn: {e}"))?;
  task.continued = true;
  let text = task_turn_output(&out.stdout, &out.stderr);
  if !out.status.success() {
    return Err(if text.is_empty() {
      format!("OpenCode task turn exited {}", out.status)
    } else {
      format!("OpenCode task turn exited {}:\n{text}", out.status)
    });
  }
  Ok(text)
}

fn task_collect_inner(task_id: String) -> Result<Vec<serde_json::Value>, String> {
  let task_guard = AGENT_TASK.lock().unwrap_or_else(|e| e.into_inner());
  let task = task_guard.as_ref().ok_or("no active OpenCode task")?;
  if task.id != task_id { return Err("OpenCode task id does not match active task".into()); }
  let current = project_files(&task.shadow_root)?;
  let names: BTreeSet<String> = task.baseline.keys().chain(current.keys()).cloned().collect();
  let mut writes = Vec::new();
  for relative in names {
    let before = task.baseline.get(&relative).cloned().unwrap_or_default();
    let after = current.get(&relative).cloned().unwrap_or_default();
    if before != after {
      writes.push(serde_json::json!({ "relative": relative, "path": relative, "previous": before, "content": after }));
    }
  }
  Ok(writes)
}

fn task_cleanup_inner(task_id: String) -> Result<(), String> {
  let mut task_guard = AGENT_TASK.lock().unwrap_or_else(|e| e.into_inner());
  let task = task_guard.take().ok_or("no active OpenCode task")?;
  if task.id != task_id {
    *task_guard = Some(task);
    return Err("OpenCode task id does not match active task".into());
  }
  crate::remove_shadow(&task.real_root, &task.shadow_root);
  Ok(())
}

/// Scan for balanced `{…}` objects **respecting JSON strings** (braces inside `content` must not end the object).
/// Naive depth counting was why 7b write proposals never parsed — TS source is full of `{`/`}`.
fn next_json_object(text: &str, from: usize) -> Option<(usize, usize)> {
  let bytes = text.as_bytes();
  let mut i = from;
  while i < bytes.len() && bytes[i] != b'{' {
    i += 1;
  }
  if i >= bytes.len() {
    return None;
  }
  let start = i;
  let mut depth = 0i32;
  let mut in_str = false;
  let mut escape = false;
  while i < bytes.len() {
    let b = bytes[i];
    if in_str {
      if escape {
        escape = false;
      } else if b == b'\\' {
        escape = true;
      } else if b == b'"' {
        in_str = false;
      }
      i += 1;
      continue;
    }
    match b {
      b'"' => in_str = true,
      b'{' => depth += 1,
      b'}' => {
        depth -= 1;
        if depth == 0 {
          return Some((start, i + 1));
        }
      }
      _ => {}
    }
    i += 1;
  }
  None
}

/// Parse model-emitted write tool JSON → proposals (no disk write).
/// Handles OpenCode-style `{ "name":"write", "arguments":{ "content", "filePath" } }`.
fn parse_emitted_write_tools(cwd: &std::path::Path, text: &str) -> Vec<serde_json::Value> {
  let mut out = Vec::new();
  let mut i = 0;
  while let Some((start, end)) = next_json_object(text, i) {
    i = end;
    let slice = &text[start..end];
    // Tool name write / write_file / create_file (not every object with the word "write")
    let looks_write = slice.contains("\"name\"")
      && (slice.contains("\"write\"")
        || slice.contains("\"write_file\"")
        || slice.contains("\"create_file\""));
    if !looks_write {
      continue;
    }
    if let Some(p) = parse_one_write(cwd, slice) {
      out.push(p);
    }
  }
  out
}

/// Pull a JSON string field even when the blob is slightly invalid (common with small models).
fn extract_json_string_field(blob: &str, field: &str) -> Option<String> {
  let key = format!("\"{field}\"");
  let idx = blob.find(&key)?;
  let after = &blob[idx + key.len()..];
  let colon = after.find(':')?;
  let mut rest = after[colon + 1..].trim_start();
  if !rest.starts_with('"') {
    return None;
  }
  rest = &rest[1..];
  let mut out = String::new();
  let mut chars = rest.chars().peekable();
  while let Some(c) = chars.next() {
    if c == '\\' {
      match chars.next() {
        Some('n') => out.push('\n'),
        Some('t') => out.push('\t'),
        Some('r') => out.push('\r'),
        Some('"') => out.push('"'),
        Some('\\') => out.push('\\'),
        Some('\'') => out.push('\''),
        Some(o) => {
          out.push('\\');
          out.push(o);
        }
        None => break,
      }
    } else if c == '"' {
      break;
    } else {
      out.push(c);
    }
  }
  if out.is_empty() && !rest.starts_with('"') {
    None
  } else {
    Some(out)
  }
}

fn parse_one_write(cwd: &std::path::Path, json_str: &str) -> Option<serde_json::Value> {
  // Models often emit invalid JSON (\' inside "..."). Soft-repair then parse; else regex fallback.
  let repaired = json_str
    .replace("\\'", "'")
    .replace("\r\n", "\n");
  let (raw_path, content) = if let Ok(v) = serde_json::from_str::<serde_json::Value>(&repaired) {
    let name = v.get("name")?.as_str()?;
    if name != "write" && name != "write_file" && name != "create_file" {
      return None;
    }
    let args = v.get("arguments").or_else(|| v.get("args"))?;
    let content = args
      .get("content")
      .or_else(|| args.get("contents"))
      .and_then(|c| c.as_str())?
      .to_string();
    let raw_path = args
      .get("filePath")
      .or_else(|| args.get("path"))
      .or_else(|| args.get("file"))
      .and_then(|p| p.as_str())?
      .to_string();
    (raw_path, content)
  } else {
    // Fallback: pull filePath + content with loose patterns
    let path = extract_json_string_field(json_str, "filePath")
      .or_else(|| extract_json_string_field(json_str, "path"))?;
    let content = extract_json_string_field(json_str, "content")
      .or_else(|| extract_json_string_field(json_str, "contents"))?;
    (path, content)
  };

  let rel = {
    let p = raw_path.trim();
    if p.contains("/path/to/") || p.starts_with("/path/") {
      std::path::Path::new(p)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("DOGFOOD-OPENCODE.md")
        .to_string()
    } else if let Ok(stripped) = std::path::Path::new(p).strip_prefix(cwd) {
      stripped.to_string_lossy().to_string()
    } else if p.starts_with('/') {
      // Prefer path relative to project if under cwd; else basename only
      if p.starts_with(&cwd.display().to_string()) {
        p[cwd.display().to_string().len()..]
          .trim_start_matches('/')
          .to_string()
      } else {
        std::path::Path::new(p)
          .file_name()
          .and_then(|s| s.to_str())
          .unwrap_or("out.txt")
          .to_string()
      }
    } else {
      p.trim_start_matches("./").to_string()
    }
  };
  if rel.is_empty() || rel.contains("..") {
    return None;
  }
  let abs = cwd.join(&rel);
  // Current on-disk content (if any) so verify/land can do full-file replace, not empty-search on non-empty.
  let previous = std::fs::read_to_string(&abs).unwrap_or_default();
  Some(serde_json::json!({
    "relative": rel,
    "path": abs.display().to_string(),
    "content": content,
    "previous": previous,
  }))
}

#[tauri::command]
pub fn opencode_status() -> OpencodeStatus {
  status_inner()
}

#[tauri::command]
pub fn opencode_start(project_path: Option<String>) -> Result<OpencodeStatus, String> {
  start_inner(project_path)
}

#[tauri::command]
pub fn opencode_stop() -> Result<OpencodeStatus, String> {
  stop_inner()
}

#[tauri::command]
pub fn opencode_ensure(project_path: Option<String>) -> Result<OpencodeStatus, String> {
  ensure_inner(project_path)
}

#[tauri::command]
pub fn opencode_run(
  project_path: Option<String>,
  message: String,
  model: Option<String>,
  ollama_base: Option<String>,
  provider: Option<OpenCodeProvider>,
) -> Result<serde_json::Value, String> {
  run_inner(project_path, message, model, ollama_base, provider)
}

#[tauri::command]
pub fn opencode_task_start(
  project_path: Option<String>,
  model: Option<String>,
  ollama_base: Option<String>,
  provider: Option<OpenCodeProvider>,
) -> Result<OpencodeTaskStart, String> {
  task_start_inner(project_path, model, ollama_base, provider)
}

#[tauri::command]
pub fn opencode_task_collect(task_id: String) -> Result<Vec<serde_json::Value>, String> {
  task_collect_inner(task_id)
}

#[tauri::command]
pub fn opencode_task_turn(task_id: String, message: String, model: Option<String>, ollama_base: Option<String>) -> Result<String, String> {
  task_turn_inner(task_id, message, model, ollama_base)
}

#[tauri::command]
pub fn opencode_task_cleanup(task_id: String) -> Result<(), String> {
  task_cleanup_inner(task_id)
}

pub fn status_json() -> String {
  serde_json::to_string(&status_inner()).unwrap_or_else(|_| "{}".into())
}

pub fn start_json(project_path: Option<String>) -> Result<String, String> {
  start_inner(project_path).map(|s| serde_json::to_string(&s).unwrap_or_else(|_| "{}".into()))
}

pub fn stop_json() -> Result<String, String> {
  stop_inner().map(|s| serde_json::to_string(&s).unwrap_or_else(|_| "{}".into()))
}

pub fn ensure_json(project_path: Option<String>) -> Result<String, String> {
  ensure_inner(project_path).map(|s| serde_json::to_string(&s).unwrap_or_else(|_| "{}".into()))
}

pub fn run_json(
  project_path: Option<String>,
  message: String,
  model: Option<String>,
  ollama_base: Option<String>,
  provider: Option<serde_json::Value>,
) -> Result<String, String> {
  let provider = provider.map(serde_json::from_value::<OpenCodeProvider>)
    .transpose().map_err(|e| format!("bad OpenCode provider: {e}"))?;
  run_inner(project_path, message, model, ollama_base, provider)
    .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()))
}

pub fn task_start_json(
  project_path: Option<String>, model: Option<String>, ollama_base: Option<String>, provider: Option<serde_json::Value>,
) -> Result<String, String> {
  let provider = provider.map(serde_json::from_value::<OpenCodeProvider>)
    .transpose().map_err(|e| format!("bad OpenCode provider: {e}"))?;
  task_start_inner(project_path, model, ollama_base, provider)
    .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()))
}

pub fn task_collect_json(task_id: Option<String>) -> Result<String, String> {
  task_collect_inner(task_id.ok_or("taskId required")?)
    .map(|v| serde_json::to_string(&v).unwrap_or_else(|_| "[]".into()))
}

pub fn task_turn_json(task_id: Option<String>, message: Option<String>, model: Option<String>, ollama_base: Option<String>) -> Result<String, String> {
  task_turn_inner(task_id.ok_or("taskId required")?, message.unwrap_or_default(), model, ollama_base)
    .map(|v| serde_json::json!({ "output": v }).to_string())
}

pub fn task_cleanup_json(task_id: Option<String>) -> Result<String, String> {
  task_cleanup_inner(task_id.ok_or("taskId required")?)
    .map(|_| serde_json::json!({ "ok": true }).to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn task_turn_output_keeps_provider_error_after_a_stdout_banner() {
    let output = task_turn_output(
      b"\x1b[0m\n> rlm-shadow \xc2\xb7 @cf/qwen\n\x1b[0m\n",
      b"Workers AI error: 8007: shared context exceeded\n",
    );
    assert!(output.contains("> rlm-shadow"));
    assert!(output.contains("[OpenCode stderr]"));
    assert!(output.contains("Workers AI error: 8007"));
  }

  #[test]
  fn task_agent_caps_tool_iterations_and_rejects_a_doom_loop() {
    let provider = TaskProvider::Ollama { base_url: None };
    let config: serde_json::Value = serde_json::from_str(
      &build_task_opencode_config("ollama/test", &provider).expect("task config"),
    ).expect("JSON config");
    assert_eq!(config["agent"]["rlm-shadow"]["steps"], 12);
    assert_eq!(config["agent"]["rlm-shadow"]["temperature"], 0.1);
    assert_eq!(config["permission"]["doom_loop"], "deny");
    assert_eq!(config["agent"]["rlm-shadow"]["permission"]["doom_loop"], "deny");
  }

  #[test]
  fn task_message_rewrites_the_real_root_and_requires_single_relative_tool_calls() {
    let root = Path::new("/home/jeff/projects/rlmlocal-site-test");
    let message = "Project root: /home/jeff/projects/rlmlocal-site-test\nRead /home/jeff/projects/rlmlocal-site-test/src/stripThinkTags.ts";
    let bounded = task_message_in_shadow(message, root);
    assert!(!bounded.contains("/home/jeff/projects/rlmlocal-site-test"));
    assert!(bounded.contains("Project root: ."));
    assert!(bounded.contains("Read ./src/stripThinkTags.ts"));
    assert!(bounded.contains("exactly one JSON argument object"));
  }

  #[test]
  fn json_object_scan_ignores_braces_inside_strings() {
    let text = r#"here {"name":"write","arguments":{"filePath":"/p/a.ts","content":"export function f() {\n  return 1;\n}\n"}} tail"#;
    let (s, e) = next_json_object(text, 0).expect("object");
    let slice = &text[s..e];
    assert!(slice.starts_with('{') && slice.ends_with('}'));
    assert!(slice.contains("export function f()"));
    // Must not close at the first `}` inside the content string
    assert!(slice.contains("return 1"));
  }

  #[test]
  fn parse_write_with_braces_in_content() {
    let dir = std::env::temp_dir().join("rlm-oc-parse-test");
    let _ = std::fs::create_dir_all(&dir);
    let text = r#"{"name":"write","arguments":{"filePath":"src/x.ts","content":"export function f() {\n  return { a: 1 };\n}\n"}}"#;
    let props = parse_emitted_write_tools(&dir, text);
    assert_eq!(props.len(), 1);
    assert_eq!(props[0]["relative"], "src/x.ts");
    assert!(props[0]["content"].as_str().unwrap().contains("return { a: 1 }"));
  }

  #[test]
  fn edge_response_becomes_openai_completion_with_tool_call() {
    let completion = edge_openai_completion(&serde_json::json!({
      "content": "",
      "tool_calls": [{
        "id": "call_1",
        "type": "function",
        "function": { "name": "read", "arguments": { "path": "src/a.ts" } }
      }]
    }), "@cf/qwen/qwen3-30b-a3b-fp8", false);

    assert_eq!(completion["object"], "chat.completion");
    assert_eq!(completion["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(completion["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "read");
    assert_eq!(completion["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"], r#"{"path":"src/a.ts"}"#);
  }

  #[test]
  fn edge_response_stream_is_valid_openai_sse_shape() {
    let completion = edge_openai_completion(&serde_json::json!({ "content": "hello" }), "@cf/test", false);
    let stream = edge_openai_sse(&completion);
    assert!(stream.contains("chat.completion.chunk"));
    assert!(stream.contains(r#""content":"hello""#));
    assert!(stream.ends_with("data: [DONE]\n\n"));
  }

  #[test]
  fn edge_stream_completion_uses_openai_delta_when_legacy_response_is_empty() {
    let worker = edge_stream_completion(concat!(
      "data: {\"response\":\"\",\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\n",
      "data: {\"response\":\"OK\",\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\n",
      "data: [DONE]\n",
    )).expect("stream completion");
    assert_eq!(worker["content"], "OK");
    assert_eq!(worker["tool_calls"], serde_json::json!([]));
  }

  #[test]
  fn edge_stream_completion_merges_fragmented_tool_arguments() {
    let worker = edge_stream_completion(concat!(
      "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"\"}}]}}]}\n\n",
      "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"src/a.ts\\\"}\"}}]}}]}\n\n",
      "data: [DONE]\n",
    )).expect("stream tool completion");
    let calls = normalize_edge_tool_calls(&worker["tool_calls"], false);
    assert_eq!(calls[0]["function"]["name"], "read");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"path":"src/a.ts"}"#);
  }

  #[test]
  fn edge_stream_completion_keeps_unindexed_complete_tool_calls_separate() {
    let worker = edge_stream_completion(concat!(
      r#"data: {"tool_calls":[{"function":{"name":"read","arguments":"{\"filePath\":\"src/a.ts\"}"}}]}"#, "\n\n",
      r#"data: {"tool_calls":[{"function":{"name":"read","arguments":"{\"filePath\":\"src/b.ts\"}"}}]}"#, "\n\n",
      "data: [DONE]\n",
    )).expect("stream tool completion");
    let calls = normalize_edge_tool_calls(&worker["tool_calls"], false);
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["function"]["arguments"], r#"{"filePath":"src/a.ts"}"#);
    assert_eq!(calls[1]["function"]["arguments"], r#"{"filePath":"src/b.ts"}"#);
  }

  #[test]
  fn edge_artifact_literal_shield_round_trips_source_and_tool_arguments() {
    let source = "return s.replace(/<think>[\\s\\S]*?<\\/think>/gi, '').trim();";
    let protected = shield_edge_artifact_literals(source);
    assert!(!protected.contains("<think>"));
    assert!(!protected.contains("</think>"));
    assert_eq!(unshield_edge_artifact_literals(&protected), source);

    let open = EDGE_ARTIFACT_LITERAL_SHIELDS[0].1;
    let close = EDGE_ARTIFACT_LITERAL_SHIELDS[1].1;
    let completion = edge_openai_completion(&serde_json::json!({
      "content": format!("source {open}hidden{close}"),
      "tool_calls": [{
        "id": "call_write",
        "function": {
          "name": "write",
          "arguments": format!(r#"{{"content":"{open}hidden{close}"}}"#),
        }
      }]
    }), "@cf/test", true);
    assert_eq!(completion["choices"][0]["message"]["content"], "source <think>hidden</think>");
    assert_eq!(
      completion["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
      r#"{"content":"<think>hidden</think>"}"#,
    );
  }

  #[test]
  fn edge_provider_uses_the_task_local_model_namespace() {
    let provider: OpenCodeProvider = serde_json::from_value(serde_json::json!({
      "kind": "edge",
      "workerURL": "https://edge.example.test",
      "ownerKey": "secret"
    })).expect("provider");
    match provider {
      OpenCodeProvider::Edge { worker_url, owner_key, context_window } => {
        assert_eq!(worker_url, "https://edge.example.test");
        assert_eq!(owner_key.as_deref(), Some("secret"));
        assert_eq!(context_window, None);
      }
      _ => panic!("expected edge provider"),
    }
  }

  #[test]
  fn edge_proxy_adapts_the_worker_contract_to_openai_chat() {
    let worker = match Server::http(("127.0.0.1", 0)) {
      Ok(server) => server,
      // The hosted Codex sandbox denies listener creation. A real Tauri
      // process owns localhost and runs this integration test normally.
      Err(error) if error.downcast_ref::<std::io::Error>()
        .map(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
        .unwrap_or(false) => return,
      Err(error) => panic!("worker mock: {error}"),
    };
    let port = worker.server_addr().to_ip().expect("worker socket").port();
    let worker_turn = std::thread::spawn(move || {
      let mut request = worker.recv_timeout(Duration::from_secs(2)).expect("worker receive").expect("worker request");
      assert_eq!(request.method(), &Method::Post);
      assert_eq!(request.url(), "/infer");
      let mut body = String::new();
      request.as_reader().read_to_string(&mut body).expect("worker body");
      let body: serde_json::Value = serde_json::from_str(&body).expect("worker JSON");
      assert_eq!(body["model"], "@cf/test");
      assert_eq!(body["max_tokens"], 8_000);
      assert_eq!(body["output_mode"], "artifact");
      assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false);
      let messages = body["messages"].as_array().expect("messages");
      assert_eq!(messages[0]["role"], "system");
      assert!(messages[1]["content"].as_str().expect("content").contains(EDGE_ARTIFACT_LITERAL_SHIELDS[0].1));
      request.respond(Response::from_string(format!(
        r#"{{"content":"edge {} reply","tool_calls":[]}}"#,
        EDGE_ARTIFACT_LITERAL_SHIELDS[0].1,
      ))
        .with_header(Header::from_bytes("Content-Type", "application/json").expect("header")))
        .expect("worker response");
    });

    let proxy = start_edge_proxy(format!("http://127.0.0.1:{port}"), None, "@cf/test".into(), Some(32_000)).expect("edge proxy");
    let response = HttpClient::new()
      .post(format!("{}/chat/completions", proxy.base_url()))
      .bearer_auth(proxy.api_key())
      .json(&serde_json::json!({
        "model": "@cf/test",
        "messages": [{ "role": "user", "content": "hello <think>" }],
        "max_tokens": 32_000
      }))
      .send().expect("proxy response");
    assert!(response.status().is_success());
    let response: serde_json::Value = response.json().expect("OpenAI JSON");
    assert_eq!(response["object"], "chat.completion");
    assert_eq!(response["choices"][0]["message"]["content"], "edge <think> reply");
    drop(proxy);
    worker_turn.join().expect("worker thread");
  }

  #[test]
  fn edge_budget_caps_opencode_default_against_shared_context() {
    // The exact failure observed in the desktop task: OpenCode requested a
    // 32k completion against the 32,768-token Qwen window.
    assert_eq!(edge_completion_budget(769 * 3, 32_000, 32_000).expect("budget"), 8_000);
    // Small/default edge models automatically get a proportionally smaller
    // task turn instead of inheriting the Qwen limit.
    assert_eq!(edge_completion_budget(100, 32_000, 7_968).expect("budget"), 1_992);
  }

}
