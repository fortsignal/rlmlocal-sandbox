use std::path::{Path, PathBuf};
use std::process::Command;
// OpenCode process host (Code door) — CLI/`serve` lifecycle; no Electron.
mod opencode_host;
use opencode_host::{
  opencode_ensure, opencode_run, opencode_start, opencode_status, opencode_stop,
  opencode_task_cleanup, opencode_task_collect, opencode_task_start, opencode_task_turn,
};

#[derive(serde::Serialize)]
struct TestResult {
  runner: String,
  code: i32,
  failed: i64,
  output: String,
}

#[derive(serde::Serialize)]
struct VerifyResult {
  runner: String,
  baseline_failed: i64,
  after_failed: i64,
  broke: bool, // baseline-diff: did THIS change introduce NEW failures (tests OR lint OR types)?
  reverted: bool,
  // #1 deterministic checks (baseline-diff): NEW lint / type errors the change introduced. Model-proof — a
  // broken refactor (e.g. calling an undefined function) shows here even when the tests don't cover that path.
  lint_new: i64,
  type_new: i64,
  output: String,
}

/// Does package.json declare a script named `name`? (Crude substring — good enough to gate `npm run <name>`.) */
fn has_npm_script(root: &Path, name: &str) -> bool {
  std::fs::read_to_string(root.join("package.json"))
    .map(|c| c.contains(&format!("\"{name}\"")))
    .unwrap_or(false)
}

/// Count error markers in a check tool's output (eslint "… error …" lines, tsc "error TS"). Used only for a
/// baseline-DIFF (after − baseline), so a constant over-count (e.g. eslint's summary line) cancels out.
fn count_errors(s: &str) -> i64 {
  let clean = strip_ansi(s);
  let tsc = clean.matches("error TS").count() as i64;
  if tsc > 0 {
    return tsc;
  }
  clean.lines().filter(|l| l.to_lowercase().contains("error")).count() as i64
}

/// #1 — run the project's OWN deterministic checks (lint + typecheck) in the given tree. Agnostic + best-effort:
/// a missing tool/script = 0 (skipped), never an error. Returns (lint_errors, type_errors, output-tail).
fn run_project_checks(root: &Path) -> (i64, i64, String) {
  let env = load_project_env(root);
  let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
  let mut out = String::new();
  // LINT — the project's own `lint` script (e.g. eslint), if it has one.
  let lint = if has_npm_script(root, "lint") {
    let mut cmd = Command::new(npm);
    cmd.args(["run", "lint", "--silent"]).current_dir(root).envs(env.iter().map(|(k, v)| (k, v)));
    match run_with_timeout(&mut cmd, CHECK_TIMEOUT) {
      Ok(o) if !o.status.success() => {
        let c = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        let n = count_errors(&c);
        out.push_str(&format!("[lint]\n{}\n", tail(&c, 800)));
        n
      }
      _ => 0,
    }
  } else {
    0
  };
  // TYPECHECK — `tsc --noEmit` when tsconfig.json is present.
  let types = if root.join("tsconfig.json").exists() {
    let npx = if cfg!(windows) { "npx.cmd" } else { "npx" };
    let mut cmd = Command::new(npx);
    cmd.args(["tsc", "--noEmit"]).current_dir(root).envs(env.iter().map(|(k, v)| (k, v)));
    match run_with_timeout(&mut cmd, CHECK_TIMEOUT) {
      Ok(o) if !o.status.success() => {
        let c = String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr);
        let n = c.matches("error TS").count() as i64;
        out.push_str(&format!("[typecheck]\n{}\n", tail(&c, 800)));
        n
      }
      _ => 0,
    }
  } else {
    0
  };
  (lint, types, out)
}

/// Any known project manifest marks a project root.
const MANIFESTS: [&str; 8] = [
  "package.json",
  "Cargo.toml",
  "go.mod",
  "pyproject.toml",
  "setup.py",
  "pytest.ini",
  "requirements.txt",
  "Makefile",
];

fn find_project_root(start: PathBuf) -> Option<PathBuf> {
  let mut dir = start;
  loop {
    if MANIFESTS.iter().any(|m| dir.join(m).exists()) {
      return Some(dir);
    }
    if !dir.pop() {
      return None;
    }
  }
}

/// Explicit path (the user's loaded folder) wins; else walk up from cwd (dogfood default).
fn resolve_root(project_path: Option<String>) -> Result<PathBuf, String> {
  match project_path {
    Some(p) if !p.is_empty() => {
      let pb = PathBuf::from(&p);
      if MANIFESTS.iter().any(|m| pb.join(m).exists()) {
        Ok(pb)
      } else {
        Err(format!("no recognizable project manifest at {p}"))
      }
    }
    _ => {
      let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
      find_project_root(cwd).ok_or_else(|| "no project root found".into())
    }
  }
}

/// LANG-REG (2026-07-19): the Rust half of the language registry — ONE table for execution facts,
/// mirroring src/features/agent/languagePacks.ts (the TS single source for detector tables). Two
/// copies exist ONLY because they live on opposite sides of the TS/Rust boundary; the TS side is
/// the editor of record — sync this when it changes.
const CODE_EXTS: &[&str] = &[
  ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts",
  ".py", ".go", ".rs", ".java", ".rb", ".php",
  ".c", ".cc", ".cpp", ".h", ".hpp", ".cs", ".swift", ".kt", ".kts", ".scala",
  ".vue", ".svelte", ".sql",
];

/// Runner selection: manifests (IN ORDER — first hit wins) → test command. The python venv
/// preference and the windows npm.cmd variant stay CODE (resolution logic, not table data).
struct RunnerPack {
  manifests: &'static [&'static str],
  prog: &'static str,
  args: &'static [&'static str],
  label: &'static str,
}
const RUNNERS: &[RunnerPack] = &[
  RunnerPack { manifests: &["package.json"], prog: "npm", args: &["test"], label: "node" },
  RunnerPack { manifests: &["Cargo.toml"], prog: "cargo", args: &["test"], label: "rust" },
  RunnerPack { manifests: &["go.mod"], prog: "go", args: &["test", "./..."], label: "go" },
  RunnerPack { manifests: &["pyproject.toml", "setup.py", "setup.cfg", "pytest.ini", "requirements.txt", "conftest.py"], prog: "pytest", args: &[], label: "python" },
  RunnerPack { manifests: &["Makefile"], prog: "make", args: &["test"], label: "make" },
];

/// AGNOSTIC runner detection — map the project's manifest to its test command (table-driven, LANG-REG).
fn detect_runner(root: &Path) -> Result<(String, Vec<String>, &'static str), String> {
  let has = |f: &str| root.join(f).exists();
  for pack in RUNNERS {
    if !pack.manifests.iter().any(|m| has(m)) {
      continue;
    }
    // Prefer the project's OWN virtualenv over a bare `pytest` (2026-07-19 audit): without it, either
    // no pytest is on PATH (honest error) or WORSE a global pytest runs WITHOUT the project's deps —
    // collection errors at baseline AND after → identical fingerprints → FALSE GREEN on a suite that
    // never ran. The venv is symlinked into the verify shadow (make_shadow), so this path exists there.
    if pack.label == "python" {
      for cand in [".venv/bin/python", "venv/bin/python", ".venv/Scripts/python.exe", "venv/Scripts/python.exe"] {
        let py = root.join(cand);
        if py.exists() {
          return Ok((py.to_string_lossy().into_owned(), vec!["-m".into(), "pytest".into()], "python"));
        }
      }
    }
    let prog = if cfg!(windows) && pack.prog == "npm" { "npm.cmd".to_string() } else { pack.prog.to_string() };
    return Ok((prog, pack.args.iter().map(|a| a.to_string()).collect(), pack.label));
  }
  Err("couldn't detect a test runner for this project".into())
}

/// Load KEY=VALUE from the project's own env files (.env, then .env.local overrides) — so the loop
/// runs tests with the SAME environment the user's own run uses. If it runs locally, the loop runs it.
fn load_project_env(root: &Path) -> Vec<(String, String)> {
  let mut vars: Vec<(String, String)> = Vec::new();
  for name in [".env", ".env.local"] {
    if let Ok(content) = std::fs::read_to_string(root.join(name)) {
      for line in content.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
          continue;
        }
        if let Some((k, v)) = l.split_once('=') {
          let key = k.trim().to_string();
          let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
          vars.retain(|(ek, _)| ek != &key);
          vars.push((key, val));
        }
      }
    }
  }
  vars
}

fn strip_ansi(s: &str) -> String {
  let mut out = String::new();
  let mut chars = s.chars();
  while let Some(c) = chars.next() {
    if c == '\u{1b}' {
      for n in chars.by_ref() {
        if n == 'm' {
          break;
        }
      }
    } else {
      out.push(c);
    }
  }
  out
}

/// Generic failure count: find "<n> failed" anywhere (vitest/jest/pytest/cargo all print it);
/// else 0 if output shows "passed"/"ok"; else None (caller falls back to exit code).
fn parse_failed(output: &str) -> Option<i64> {
  let clean = strip_ansi(output);
  for (i, _) in clean.match_indices("failed") {
    let trimmed = clean[..i].trim_end();
    let digits: String = trimmed
      .chars()
      .rev()
      .take_while(|c| c.is_ascii_digit())
      .collect::<String>()
      .chars()
      .rev()
      .collect();
    if let Ok(n) = digits.parse::<i64>() {
      return Some(n);
    }
  }
  if clean.contains("passed") || clean.contains(" ok ") || clean.contains("test result: ok") {
    return Some(0);
  }
  None
}

/// std::process has no timeout — a watch-mode or deadlocked test script would freeze verify FOREVER
/// (2026-07-19 audit). Spawn with stdout/stderr drained on threads (so a chatty suite can't deadlock
/// on a full pipe), poll try_wait, kill past the deadline. A timeout is an ERROR — surfaces RED,
/// never silently green.
fn run_with_timeout(cmd: &mut Command, timeout: std::time::Duration) -> std::io::Result<std::process::Output> {
  use std::io::Read;
  let mut child = cmd.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn()?;
  let mut out_pipe = child.stdout.take().expect("piped stdout");
  let mut err_pipe = child.stderr.take().expect("piped stderr");
  let t_out = std::thread::spawn(move || { let mut b = Vec::new(); let _ = out_pipe.read_to_end(&mut b); b });
  let t_err = std::thread::spawn(move || { let mut b = Vec::new(); let _ = err_pipe.read_to_end(&mut b); b });
  let start = std::time::Instant::now();
  let status = loop {
    if let Some(s) = child.try_wait()? { break s; }
    if start.elapsed() > timeout {
      let _ = child.kill();
      let _ = child.wait();
      let _ = t_out.join();
      let _ = t_err.join();
      return Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("command timed out after {}s", timeout.as_secs()),
      ));
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
  };
  let stdout = t_out.join().unwrap_or_default();
  let stderr = t_err.join().unwrap_or_default();
  Ok(std::process::Output { status, stdout, stderr })
}

/// Suites get longer (a cold `cargo test` recompile can take minutes); deterministic checks are quick.
const SUITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Run the project's suite once, runner auto-detected, env loaded.
/// Returns (runner, exit_code, failed_count, combined-output).
fn run_suite(root: &Path) -> Result<(String, i32, i64, String), String> {
  let (prog, args, label) = detect_runner(root)?;
  let env = load_project_env(root);
  let mut cmd = Command::new(&prog);
  cmd.args(&args).current_dir(root).envs(env);
  let output = run_with_timeout(&mut cmd, SUITE_TIMEOUT).map_err(|e| format!("failed to run {prog}: {e}"))?;
  let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
  combined.push_str(&String::from_utf8_lossy(&output.stderr));
  let code = output.status.code().unwrap_or(-1);
  let failed = parse_failed(&combined).unwrap_or(if code == 0 { 0 } else { 1 });
  Ok((label.to_string(), code, failed, combined))
}

fn tail(s: &str, n: usize) -> String {
  let chars: Vec<char> = s.chars().collect();
  let start = chars.len().saturating_sub(n);
  chars[start..].iter().collect()
}

/// Run the LOADED project's test suite (runner + env auto-detected). The outcome signal. Shared logic so
/// the native `invoke` path (the shipped app) and the dev HTTP bridge (external browser) produce the SAME
/// result from the SAME code — never two implementations to drift.
fn run_tests_inner(project_path: Option<String>) -> Result<TestResult, String> {
  let root = resolve_root(project_path)?;
  let (runner, code, failed, out) = run_suite(&root)?;
  Ok(TestResult {
    runner,
    code,
    failed,
    output: tail(&out, 2000),
  })
}

#[tauri::command]
fn run_tests(project_path: Option<String>) -> Result<TestResult, String> {
  run_tests_inner(project_path)
}

pub(crate) fn now_ms() -> u128 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_millis())
    .unwrap_or(0)
}

/// Create a SHADOW of the project via `git worktree` so verification NEVER touches the real source.
/// node_modules is symlinked (copying GBs is absurd); gitignored env files are copied in so the shadow
/// runs with the same env. The shadow checks out HEAD — the real working tree is read-only, untouched.
pub(crate) fn make_shadow(real_root: &Path) -> Result<PathBuf, String> {
  let real = real_root.to_str().ok_or("non-utf8 project path")?;
  let shadow = std::env::temp_dir().join(format!("rlm-shadow-{}-{}", std::process::id(), now_ms()));
  let shadow_s = shadow.to_str().ok_or("non-utf8 temp path")?;

  let out = Command::new("git")
    .args(["-C", real, "worktree", "add", "--detach", shadow_s, "HEAD"])
    .output()
    .map_err(|e| format!("git worktree (is this a git repo?): {e}"))?;
  if !out.status.success() {
    return Err(format!(
      "git worktree add failed (shadow verify needs a git repo): {}",
      String::from_utf8_lossy(&out.stderr)
    ));
  }

  // Overlay the real WORKING TREE onto the HEAD shadow so verify reflects what the brain (and user) actually see —
  // not just the last commit. apply_patch LANDS to the working tree (uncommitted), so without this every landed
  // change is invisible to the next verify ("search not found") and the refactor loop drifts apart after one land.
  // Tracked edits: apply the HEAD→worktree diff. Untracked, non-ignored files (e.g. a new module a prior move
  // created): copy them in. Best-effort — on any failure the shadow simply stays at HEAD (the old behavior).
  // Tracked edits: COPY each changed file straight from the real working tree. (Was `git diff HEAD | git apply`,
  // which could fail silently on a large/complex diff and leave the shadow at HEAD — so a prior land's anchor was
  // "search not found" on the next verify, breaking land→verify→land. 2026-06-22: 15 landed move-alones → the
  // overlay fell back → batch couldn't find `import { ollamaCtx }`.) A direct copy can't mis-apply; deletes drop.
  if let Ok(changed) = Command::new("git").args(["-C", real, "diff", "HEAD", "--name-only"]).output() {
    if changed.status.success() {
      for rel in String::from_utf8_lossy(&changed.stdout).lines() {
        let rel = rel.trim();
        if rel.is_empty() { continue; }
        let src = real_root.join(rel);
        let dst = shadow.join(rel);
        if src.exists() {
          if let Some(parent) = dst.parent() { let _ = std::fs::create_dir_all(parent); }
          let _ = std::fs::copy(&src, &dst);
        } else {
          let _ = std::fs::remove_file(&dst); // deleted in the working tree → drop from the shadow too
        }
      }
    }
  }
  if let Ok(others) = Command::new("git").args(["-C", real, "ls-files", "--others", "--exclude-standard"]).output() {
    for rel in String::from_utf8_lossy(&others.stdout).lines() {
      let rel = rel.trim();
      if rel.is_empty() { continue; }
      let dst = shadow.join(rel);
      if let Some(parent) = dst.parent() { let _ = std::fs::create_dir_all(parent); }
      let _ = std::fs::copy(real_root.join(rel), &dst);
    }
  }

  // deps: symlink, never copy
  let nm = real_root.join("node_modules");
  if nm.exists() {
    let link = shadow.join("node_modules");
    #[cfg(unix)]
    {
      let _ = std::os::unix::fs::symlink(&nm, &link);
    }
    #[cfg(windows)]
    {
      let _ = std::os::windows::fs::symlink_dir(&nm, &link);
    }
  }
  // python venvs: symlink like node_modules (2026-07-19 audit) — gitignored, so otherwise absent from
  // the shadow; then a global pytest collection-errors at BOTH baseline and after → identical
  // fingerprints → FALSE GREEN on a suite that never ran. detect_runner prefers these when present.
  for d in [".venv", "venv"] {
    let src = real_root.join(d);
    if src.exists() {
      let link = shadow.join(d);
      #[cfg(unix)]
      {
        let _ = std::os::unix::fs::symlink(&src, &link);
      }
      #[cfg(windows)]
      {
        let _ = std::os::windows::fs::symlink_dir(&src, &link);
      }
    }
  }
  // gitignored env files aren't in HEAD — copy them so the shadow runs with the real env
  for f in [".env", ".env.local"] {
    let src = real_root.join(f);
    if src.exists() {
      let _ = std::fs::copy(&src, shadow.join(f));
    }
  }
  Ok(shadow)
}

/// Always discard the shadow (git worktree remove + rm -rf belt-and-suspenders).
pub(crate) fn remove_shadow(real_root: &Path, shadow: &Path) {
  if let (Some(real), Some(s)) = (real_root.to_str(), shadow.to_str()) {
    let _ = Command::new("git")
      .args(["-C", real, "worktree", "remove", "--force", s])
      .output();
  }
  let _ = std::fs::remove_dir_all(shadow);
}

/// VERIFY HOP — runs ENTIRELY in a git-worktree SHADOW; the real source is NEVER touched. baseline-diff:
/// run the clean shadow → apply the change in the shadow → run again → discard the shadow. `broke` is true
/// only if the change introduced NEW failures. The ONLY thing that writes the real tree is `apply_patch`,
/// and that is called only by the cockpit's Approve. Agnostic across node/python/go/rust/make.
/// SET of failure FINGERPRINTS from a runner's output — failing-file/test lines, normalized (digits/timings
/// stripped). Set-DIFF (after − baseline) catches a NEW failure even when the COUNT coincidentally matches a
/// pre-existing, DIFFERENT failure (the gap that let a syntax error pass as "1→1"). Conservative markers only.
fn failure_fingerprints(output: &str) -> std::collections::HashSet<String> {
  let clean = strip_ansi(output);
  let mut set = std::collections::HashSet::new();
  for line in clean.lines() {
    let t = line.trim();
    let is_fail = t.starts_with('×')
      || t.starts_with('✗')
      || t.starts_with('✕')
      || t.starts_with('❯') // vitest failing-file marker
      || t.starts_with("FAIL")
      || t.starts_with("FAILED")
      || t.contains("--- FAIL"); // go
    if is_fail {
      let norm: String = t
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
      if norm.len() > 4 {
        set.insert(norm);
      }
    }
  }
  set
}

/// PARSE CHECK — does the changed file even parse after the edit? `node --check` (JS) / `py_compile` (Python)
/// catch a syntax error (e.g. the `let score` + `const { score }` redeclaration) INSTANTLY — a class the
/// count-based test diff can miss. .ts is covered by tsc in run_project_checks; other languages fall back to
/// the test run. Returns the error text on failure, None on pass/skip.
/// Syntax-check command per extension (LANG-REG table): base args only — the file is appended at the call site.
fn parse_check_cmd(ext: &str) -> Option<(&'static str, Vec<&'static str>)> {
  match ext {
    "js" | "mjs" | "cjs" | "jsx" => Some(("node", vec!["--check"])),
    "py" => Some(("python3", vec!["-m", "py_compile"])),
    _ => None,
  }
}
fn parse_check(root: &Path, file: &str) -> Option<String> {
  let ext = Path::new(file).extension().and_then(|e| e.to_str()).unwrap_or("");
  let (prog, mut args) = parse_check_cmd(ext)?;
  args.push(file);
  match Command::new(prog).args(&args).current_dir(root).output() {
    Ok(o) if !o.status.success() => {
      let err = String::from_utf8_lossy(&o.stderr).to_string();
      Some(format!("⛔ {file} does not parse:\n{}", tail(&err, 600)))
    }
    _ => None,
  }
}

/// One verbatim edit (a SEARCH/REPLACE hunk). MULTI-HUNK: a real refactor (extract a function + update its
/// usages) is several of these, applied ATOMICALLY so the file is never left half-edited (e.g. a removed def
/// with dangling usages). They're verified together — one shadow run over the whole batch.
#[derive(serde::Deserialize, Clone)]
struct Edit {
  #[serde(default)]
  file: Option<String>, // MULTI-FILE: which file this hunk targets (None = the batch's default `file`)
  search: String,
  replace: String,
}

/// Single search/replace OR a batch of edits → one normalized list (the single path stays back-compatible).
fn edit_list(search: Option<String>, replace: Option<String>, edits: Option<Vec<Edit>>) -> Vec<Edit> {
  match edits {
    Some(e) if !e.is_empty() => e,
    _ => vec![Edit { file: None, search: search.unwrap_or_default(), replace: replace.unwrap_or_default() }],
  }
}

/// MULTI-FILE: group hunks by their target file (None → the default). A cross-file refactor (extract to a
/// module + update callers' imports) becomes several per-file groups, all applied + verified ATOMICALLY —
/// because the intermediate states (one file changed, the others not) don't compile/run.
fn group_edits_by_file(default: &str, edits: &[Edit]) -> Vec<(String, Vec<Edit>)> {
  use std::collections::BTreeMap;
  let mut map: BTreeMap<String, Vec<Edit>> = BTreeMap::new();
  for e in edits {
    let f = e.file.clone().unwrap_or_else(|| default.to_string());
    map.entry(f).or_default().push(e.clone());
  }
  map.into_iter().collect()
}

/// Apply every hunk to `content` in order — ATOMIC: if any search isn't found, the whole batch fails (no
/// partial edit). First-occurrence replace per hunk (the hunks should target distinct regions).
fn apply_edits(content: &str, edits: &[Edit]) -> Result<String, String> {
  let mut out = content.to_string();
  for (i, e) in edits.iter().enumerate() {
    // empty search = CREATE / set the file's content — valid only when the file is new/empty (the new module an
    // extract-to-module writes). On a non-empty file an empty search is ambiguous → reject.
    if e.search.is_empty() {
      if out.is_empty() { out = e.replace.clone(); continue; }
      return Err(format!("hunk {}: empty search on a non-empty file", i + 1));
    }
    if !out.contains(&e.search) {
      let head = e.search.lines().next().unwrap_or("(empty)");
      return Err(format!("hunk {} search not found: {head}", i + 1));
    }
    out = out.replacen(&e.search, &e.replace, 1);
  }
  Ok(out)
}

/// Baseline = (failing-test count, failure fingerprints, lint errors, type errors) for a clean project@HEAD.
/// CACHED so the iterate-loop's repeated passes (same HEAD) don't re-run the baseline suite each time — it's
/// identical until HEAD moves. Keyed by root@HEAD-hash, so a commit invalidates it.
type Baseline = (i64, std::collections::HashSet<String>, i64, i64, std::collections::HashSet<String>);

/// Normalized error lines from a check tool's output (drop the leading "line:col" so a pre-existing error at a
/// SHIFTED line still matches). Used to DIFF after-vs-baseline so feedback shows only the errors THIS change
/// introduced — not the project's pre-existing lint debt (e.g. 400+ frontend errors that aren't its fault).
fn check_error_set(output: &str) -> std::collections::HashSet<String> {
  strip_ansi(output)
    .lines()
    .filter(|l| l.to_lowercase().contains("error"))
    .map(|l| l.trim().trim_start_matches(|c: char| c.is_ascii_digit() || c == ':' || c == ' ').to_string())
    .collect()
}
/// The error lines present AFTER but NOT at baseline (the ones this change introduced), with their real text.
fn new_check_errors(after: &str, baseline: &std::collections::HashSet<String>) -> String {
  let mut out: Vec<String> = Vec::new();
  for l in strip_ansi(after).lines() {
    if !l.to_lowercase().contains("error") {
      continue;
    }
    let norm = l.trim().trim_start_matches(|c: char| c.is_ascii_digit() || c == ':' || c == ' ').to_string();
    if !baseline.contains(&norm) {
      out.push(l.trim().to_string());
    }
  }
  out.join("\n")
}
fn baseline_cache() -> &'static std::sync::Mutex<std::collections::HashMap<String, Baseline>> {
  static C: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Baseline>>> = std::sync::OnceLock::new();
  C.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}
fn head_hash(root: &Path) -> String {
  Command::new("git")
    .args(["rev-parse", "HEAD"])
    .current_dir(root)
    .output()
    .ok()
    .filter(|o| o.status.success())
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    .unwrap_or_default()
}

fn verify_patch_inner(
  project_path: Option<String>,
  file: String,
  edits: Vec<Edit>,
) -> Result<VerifyResult, String> {
  let root = resolve_root(project_path)?;
  let shadow = make_shadow(&root)?;

  let outcome = (|| -> Result<VerifyResult, String> {
    // MULTI-FILE: group hunks by file, PRE-APPLY each (validates every search up front → atomic across files;
    // a cross-file refactor's intermediate states don't compile, so we apply all then run ONE verify).
    let groups = group_edits_by_file(&file, &edits);
    let mut writes: Vec<(std::path::PathBuf, String, String)> = Vec::new(); // (path, patched, relfile)
    for (f, fedits) in &groups {
      let p = shadow.join(f);
      let original = std::fs::read_to_string(&p).unwrap_or_default(); // new file (extract-to-module) → empty original
      let patched = apply_edits(&original, fedits)?;
      writes.push((p, patched, f.clone()));
    }
    // baseline (CACHED per project@HEAD — identical across the iterate-loop's passes, so compute ONCE).
    let bkey = format!("{}@{}", root.display(), head_hash(&root));
    let (baseline_failed, base_fp, base_lint, base_type, base_check_set) =
      match baseline_cache().lock().ok().and_then(|c| c.get(&bkey).cloned()) {
        Some(v) => v,
        None => {
          let (_r, _bc, bf, base_out) = run_suite(&shadow)?;
          let (bl, bt, base_checks) = run_project_checks(&shadow);
          let v: Baseline = (bf, failure_fingerprints(&base_out), bl, bt, check_error_set(&base_checks));
          if let Ok(mut c) = baseline_cache().lock() {
            c.insert(bkey.clone(), v.clone());
          }
          v
        }
      };
    // apply ALL files IN THE SHADOW only, parse-check each, re-run (the after-run gives us the runner name)
    for (p, patched, f) in &writes {
      if let Some(parent) = p.parent() { let _ = std::fs::create_dir_all(parent); } // new module's dir, if any
      std::fs::write(p, patched).map_err(|e| format!("write shadow {f}: {e}"))?;
    }
    let mut parse_err: Option<String> = None;
    for (_p, _patched, f) in &writes {
      if parse_err.is_none() {
        parse_err = parse_check(&shadow, f);
      }
    }
    let (runner, _ac, after_failed, after_out) = run_suite(&shadow)?;
    let (after_lint, after_type, checks_out) = run_project_checks(&shadow);
    // SET-DIFF: failures present AFTER but NOT at baseline = the change broke them — even if the COUNT
    // coincidentally matches a pre-existing, DIFFERENT failure (the gap that let a syntax error pass as 1→1).
    let after_fp = failure_fingerprints(&after_out);
    let new_failures: Vec<&String> = after_fp.difference(&base_fp).collect();
    let lint_new = (after_lint - base_lint).max(0);
    let type_new = (after_type - base_type).max(0);
    let broke = after_failed > baseline_failed
      || !new_failures.is_empty()
      || lint_new > 0
      || type_new > 0
      || parse_err.is_some();
    // surface the parse error + any NEW failing tests FIRST so the "what failed" box leads with them.
    let mut out = String::new();
    if let Some(pe) = &parse_err {
      out.push_str(pe);
      out.push('\n');
    }
    if !new_failures.is_empty() {
      out.push_str("NEW failing tests (not failing at baseline):\n");
      for f in &new_failures {
        out.push_str(&format!("  - {f}\n"));
      }
    }
    // Only the NEW lint/type errors (diffed vs baseline) — NOT the project's pre-existing debt (e.g. 400+
    // frontend errors). Keeps the feedback + the human's "what failed" box on what THIS change broke.
    let new_checks = new_check_errors(&checks_out, &base_check_set);
    if !new_checks.is_empty() {
      out.push_str("NEW lint/type errors (introduced by this change):\n");
      out.push_str(&new_checks);
      out.push('\n');
    }
    out.push_str(&tail(&after_out, 1200)); // test-runner detail (the assertion), tailed — not the full lint dump
    // Append (not prepend) staged excerpts so the outer tail below preserves
    // this evidence alongside a noisy test runner's failure output.
    if broke {
      out.push_str("\nSTAGED source excerpts (shadow only):\n");
      for (_p, patched, f) in &writes {
        let excerpt: String = patched.chars().take(700).collect();
        out.push_str(&format!("--- {f} ---\n{excerpt}\n"));
      }
    }
    Ok(VerifyResult {
      runner,
      baseline_failed,
      after_failed,
      broke,
      reverted: true, // real source was never touched — the shadow is discarded
      lint_new,
      type_new,
      output: tail(&out, 2500),
    })
  })();

  remove_shadow(&root, &shadow); // ALWAYS discard the shadow
  outcome
}

#[tauri::command]
fn verify_patch(
  project_path: Option<String>,
  file: String,
  search: Option<String>,
  replace: Option<String>,
  edits: Option<Vec<Edit>>,
) -> Result<VerifyResult, String> {
  verify_patch_inner(project_path, file, edit_list(search, replace, edits))
}

#[derive(serde::Serialize)]
struct ApplyResult {
  applied: bool,
  file: String,
}

/// LAND a change for real (no revert) — called ONLY after the cockpit approves it. Freshness then
/// re-heals the graph for the changed file. This is the seam where human approval becomes a write.
fn apply_patch_inner(
  project_path: Option<String>,
  file: String,
  edits: Vec<Edit>,
  message: Option<String>,
) -> Result<ApplyResult, String> {
  let root = resolve_root(project_path)?;
  // MULTI-FILE: pre-apply EVERY file (validate all searches) BEFORE writing anything — atomic across files,
  // so a land never leaves the project half-edited (one file changed, a referenced one not).
  let groups = group_edits_by_file(&file, &edits);
  let mut writes: Vec<(std::path::PathBuf, String)> = Vec::new();
  for (f, fedits) in &groups {
    let p = root.join(f);
    let original = std::fs::read_to_string(&p).unwrap_or_default(); // new file (extract-to-module) → empty original
    writes.push((p, apply_edits(&original, fedits)?));
  }
  for (p, patched) in &writes {
    if let Some(parent) = p.parent() { let _ = std::fs::create_dir_all(parent); } // new module's dir, if any
    std::fs::write(p, patched).map_err(|e| format!("write {}: {e}", p.display()))?;
  }
  // COMMIT-ON-LAND: snapshot this approved change as ONE git commit (the proposal's spec is the message). Keeps
  // HEAD current so the verify shadow (a worktree at HEAD) always reflects prior lands — no "search not found" on
  // the next verify — AND gives an auditable, revertable refactor history (one commit per landed refactor).
  // Best-effort: a non-git project or a commit failure (e.g. no user.name) never fails the land — files are written,
  // and the copy-based shadow overlay still reflects the uncommitted working tree as a fallback.
  if root.join(".git").exists() {
    let msg = message.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| "rlm: landed refactor".to_string());
    let root_s = root.to_str().unwrap_or_default();
    let _ = Command::new("git").args(["-C", root_s, "add", "-A"]).output();
    let _ = Command::new("git").args(["-C", root_s, "commit", "-m", &msg, "--no-verify"]).output();
  }
  Ok(ApplyResult { applied: true, file })
}

#[tauri::command]
fn apply_patch(
  project_path: Option<String>,
  file: String,
  search: Option<String>,
  replace: Option<String>,
  edits: Option<Vec<Edit>>,
  message: Option<String>,
) -> Result<ApplyResult, String> {
  apply_patch_inner(project_path, file, edit_list(search, replace, edits), message)
}

/// A 0-based source position (tree-sitter coords) the brain asks the TS compiler about.
#[derive(serde::Deserialize, serde::Serialize, Clone)]
struct Position {
  line: i64,
  character: i64,
}

/// GROUND-TRUTH param TYPES via the project's OWN TypeScript compiler — the deterministic-extraction half that
/// hand-rolled AST inference kept getting wrong. Runs the embedded resolver (`scripts/resolveExtractTypes.cjs`,
/// baked into the binary with `include_str!`) under `node`, with the PROJECT's `node_modules` on `NODE_PATH` so
/// `require('typescript')` + the project's `tsconfig` load. Read-only. Returns the script's JSON `{ "types": […] }`
/// verbatim (or `{ "error" }`, which the brain tolerates → falls back to AST). Never writes to the project.
fn resolve_types_inner(project_path: Option<String>, file: String, positions: Vec<Position>, content: Option<String>) -> Result<String, String> {
  let root = project_path.ok_or_else(|| "resolve_types: no project path".to_string())?;
  // `content` (converge only) = VIRTUAL file content the script overlays instead of reading stale disk. null = disk.
  let input = serde_json::json!({ "root": root, "file": file, "positions": positions, "content": content }).to_string();
  run_ts_script(&root, input)
}

/// Run the embedded TS-compiler service (`scripts/resolveExtractTypes.cjs`, baked with `include_str!`) under `node`
/// with the PROJECT's `node_modules` on `NODE_PATH`, feeding `input` JSON on stdin. SHARED by both ops —
/// `resolve_types` (type strings) AND `compute_refactor` (refactor edits) — one script, one runner. Read-only.
fn run_ts_script(root: &str, input: String) -> Result<String, String> {
  const SCRIPT: &str = include_str!("../../scripts/resolveExtractTypes.cjs");
  let script_path = std::env::temp_dir().join("rlm-resolve-extract-types.cjs");
  std::fs::write(&script_path, SCRIPT).map_err(|e| format!("write ts-service: {e}"))?;
  let node = if cfg!(windows) { "node.exe" } else { "node" };
  let mut child = Command::new(node)
    .arg(&script_path)
    .current_dir(root)
    .env("NODE_PATH", format!("{root}/node_modules"))
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .map_err(|e| format!("spawn node (is it on PATH?): {e}"))?;
  if let Some(mut sin) = child.stdin.take() {
    use std::io::Write;
    let _ = sin.write_all(input.as_bytes()); // sin drops here → stdin EOF → the script reads its request
  }
  let out = child.wait_with_output().map_err(|e| format!("node run: {e}"))?;
  let s = String::from_utf8_lossy(&out.stdout).to_string();
  if s.trim().is_empty() {
    let err: String = String::from_utf8_lossy(&out.stderr).chars().take(300).collect();
    return Err(format!("ts-service no output: {err}"));
  }
  Ok(s)
}

#[tauri::command]
fn resolve_types(project_path: Option<String>, file: String, positions: Vec<Position>, content: Option<String>) -> Result<serde_json::Value, String> {
  let s = resolve_types_inner(project_path, file, positions, content)?;
  serde_json::from_str(&s).map_err(|e| format!("parse resolver output: {e}"))
}

/// REFACTOR EDITS via the project's own TS LANGUAGE SERVICE — move/extract/rename, reference-aware + cross-file (the
/// mechanics the hand-rolled mover refuses). MIRRORS `resolve_types`: same baked script, same runner, just the
/// `refactor` op. `args` passes through (move `{line}`, extract `{startLine,endLine}`, rename `{line,oldName,newName}`).
/// Returns `{ ok, edits:[{file,search,replace}] }` — the SAME Edit shape verify/apply already use. Read-only: it only
/// COMPUTES; the brain verifies + the human approves + apply_patch is the sole writer.
fn compute_refactor_inner(project_path: Option<String>, file: String, kind: String, args: serde_json::Value, content: Option<String>) -> Result<String, String> {
  let root = project_path.ok_or_else(|| "compute_refactor: no project path".to_string())?;
  // `content` (converge only) = VIRTUAL file content the script overlays instead of reading stale disk. null = disk.
  let input = serde_json::json!({ "root": root, "file": file, "op": "refactor", "kind": kind, "args": args, "content": content }).to_string();
  run_ts_script(&root, input)
}

#[tauri::command]
fn compute_refactor(project_path: Option<String>, file: String, kind: String, args: serde_json::Value, content: Option<String>) -> Result<serde_json::Value, String> {
  let s = compute_refactor_inner(project_path, file, kind, args, content)?;
  serde_json::from_str(&s).map_err(|e| format!("parse refactor output: {e}"))
}

/// CO-CHANGE coupling from git history — files that change TOGETHER across commits are functionally coupled even
/// without an import/call edge (the semantic layer fuses this with imports+calls). Reuses the existing
/// `Command::new("git")` pattern (like make_shadow / commit-on-land). `git log --name-only` → count file pairs that
/// co-occur → keep pairs above a threshold. Read-only. {} for a non-git project. Paths are repo-relative.
fn git_cochange_inner(project_path: Option<String>) -> Result<std::collections::HashMap<String, Vec<String>>, String> {
  use std::collections::HashMap;
  let root = project_path.ok_or_else(|| "git_cochange: no project path".to_string())?;
  if !std::path::Path::new(&root).join(".git").exists() { return Ok(HashMap::new()); }
  // %x01 separates commits; --name-only lists each commit's changed files.
  let out = Command::new("git")
    .args(["-C", &root, "log", "--no-merges", "--name-only", "--pretty=format:%x01", "-n", "1500"])
    .output()
    .map_err(|e| format!("git log (is this a git repo?): {e}"))?;
  let text = String::from_utf8_lossy(&out.stdout);
  let is_code = |f: &str| CODE_EXTS.iter().any(|e| f.ends_with(e));
  let mut pair_counts: HashMap<(String, String), u32> = HashMap::new();
  for block in text.split('\u{1}') {
    let files: Vec<String> = block.lines().map(|l| l.trim()).filter(|l| !l.is_empty() && is_code(l)).map(|s| s.to_string()).collect();
    // skip 1-file commits (no co-change) and huge commits (rename/format sweeps = noise, not coupling).
    if files.len() < 2 || files.len() > 25 { continue; }
    for i in 0..files.len() {
      for j in (i + 1)..files.len() {
        let (a, b) = if files[i] < files[j] { (files[i].clone(), files[j].clone()) } else { (files[j].clone(), files[i].clone()) };
        *pair_counts.entry((a, b)).or_insert(0) += 1;
      }
    }
  }
  // keep pairs that co-changed >= 3 times (a pattern, not coincidence); emit both directions.
  let mut map: HashMap<String, Vec<String>> = HashMap::new();
  for ((a, b), c) in pair_counts {
    if c >= 3 {
      map.entry(a.clone()).or_default().push(b.clone());
      map.entry(b).or_default().push(a);
    }
  }
  Ok(map)
}

#[tauri::command]
fn git_cochange(project_path: Option<String>) -> Result<serde_json::Value, String> {
  let pairs = git_cochange_inner(project_path)?;
  Ok(serde_json::json!({ "pairs": pairs }))
}

/// Exact source-literal result used only as a read-only planner backstop.
/// It does not build a second graph, embed files, or grant write authority.
#[derive(serde::Serialize)]
struct LiteralTarget {
  path: String,
  hits: usize,
}

const LITERAL_SCAN_FILE_LIMIT: usize = 12_000;
const LITERAL_SCAN_MAX_FILE_BYTES: u64 = 2_000_000;

fn literal_source_file(path: &Path) -> bool {
  matches!(
    path.extension().and_then(|extension| extension.to_str()).map(|extension| extension.to_ascii_lowercase()).as_deref(),
    Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "py" | "rs" | "go" | "java" | "kt" | "rb" | "php" | "cs" | "swift" | "vue" | "svelte")
  )
}

fn skip_literal_scan_dir(name: &str) -> bool {
  matches!(name, ".git" | "node_modules" | "dist" | "build" | "coverage" | ".next" | ".nuxt" | ".cache" | "target" | "vendor" | ".venv" | "venv" | "__pycache__")
}

fn collect_literal_source_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
  if out.len() >= LITERAL_SCAN_FILE_LIMIT { return Ok(()); }
  for entry in std::fs::read_dir(dir).map_err(|error| format!("read source directory {}: {error}", dir.display()))? {
    let entry = entry.map_err(|error| format!("read source entry: {error}"))?;
    let path = entry.path();
    let file_type = entry.file_type().map_err(|error| format!("inspect source entry {}: {error}", path.display()))?;
    if file_type.is_dir() {
      if !skip_literal_scan_dir(&entry.file_name().to_string_lossy()) {
        collect_literal_source_files(root, &path, out)?;
      }
    } else if file_type.is_file() && literal_source_file(&path) && path.strip_prefix(root).is_ok() {
      out.push(path);
    }
    if out.len() >= LITERAL_SCAN_FILE_LIMIT { break; }
  }
  Ok(())
}

/// Git gives the authoritative tracked + untracked non-ignored source set.
/// A manifest-bearing non-git folder still works through a bounded fallback.
fn literal_source_files(root: &Path) -> Result<Vec<PathBuf>, String> {
  let git_paths = Command::new("git")
    .args(["-C", root.to_str().ok_or("non-utf8 project path")?, "ls-files", "-z", "--cached", "--others", "--exclude-standard"])
    .output();
  if let Ok(output) = git_paths {
    if output.status.success() {
      let mut files = Vec::new();
      for raw in output.stdout.split(|byte| *byte == 0) {
        if raw.is_empty() { continue; }
        let relative = match std::str::from_utf8(raw) { Ok(path) => path, Err(_) => continue };
        let path = root.join(relative);
        if path.is_file() && literal_source_file(&path) { files.push(path); }
        if files.len() >= LITERAL_SCAN_FILE_LIMIT { break; }
      }
      files.sort();
      return Ok(files);
    }
  }
  let mut files = Vec::new();
  collect_literal_source_files(root, root, &mut files)?;
  files.sort();
  Ok(files)
}

fn find_literal_targets_at_root(root: &Path, literals: &[String]) -> Result<Vec<LiteralTarget>, String> {
  let literals: Vec<String> = literals.iter()
    .map(|literal| literal.trim().replace("\\/", "/").to_ascii_lowercase())
    .filter(|literal| !literal.is_empty())
    .collect();
  if literals.is_empty() { return Ok(Vec::new()); }

  let mut matches = Vec::new();
  for path in literal_source_files(root)? {
    let metadata = match std::fs::metadata(&path) { Ok(metadata) => metadata, Err(_) => continue };
    if metadata.len() > LITERAL_SCAN_MAX_FILE_BYTES { continue; }
    let source = match std::fs::read_to_string(&path) {
      Ok(source) => source,
      Err(error) if error.kind() == std::io::ErrorKind::InvalidData => continue,
      Err(_) => continue,
    };
    let normalized = source.replace("\\/", "/").to_ascii_lowercase();
    let hits = literals.iter().filter(|literal| normalized.contains(literal.as_str())).count();
    if hits == 0 { continue; }
    let relative = match path.strip_prefix(root) { Ok(relative) => relative, Err(_) => continue };
    matches.push(LiteralTarget { path: relative.to_string_lossy().replace('\\', "/"), hits });
  }
  Ok(matches)
}

fn find_literal_targets_inner(project_path: Option<String>, literals: Vec<String>) -> Result<Vec<LiteralTarget>, String> {
  let root = resolve_root(project_path)?;
  find_literal_targets_at_root(&root, &literals)
}

#[tauri::command]
fn find_literal_targets(project_path: Option<String>, literals: Vec<String>) -> Result<Vec<LiteralTarget>, String> {
  find_literal_targets_inner(project_path, literals)
}

/// The HTTP bridge's port. Frontend (Vite) stays on 5173; the Rust backend bridge gets its OWN port.
/// They are SEPARATE ports on purpose: 5173 serves the UI, 1421 is the backend the browser frontend calls.
const BRIDGE_PORT: u16 = 1421;

/// PAIRING TOKEN (hardening pt 3) — a per-launch secret the web side must send as `X-RLM-Token`. Lazily
/// generated (16 random bytes → hex). The executor shows it (log + `get_pairing_code`); the user pairs once.
/// Enforced in RELEASE only — debug builds skip it so the local dev loop (Chrome 5173) stays frictionless.
fn exec_token() -> &'static str {
  static T: std::sync::OnceLock<String> = std::sync::OnceLock::new();
  T.get_or_init(|| {
    let mut buf = [0u8; 16];
    let _ = getrandom::getrandom(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
  })
}

/// The pairing code the cockpit shows (the user pastes it into rlmlocal.com to authorize this executor).
#[tauri::command]
fn get_pairing_code() -> String {
  exec_token().to_string()
}

/// Domain-locked CORS (hardening spec pt 2): is this request Origin allowed to call the bridge? DEV =
/// any localhost / 127.0.0.1 (Vite :5173, Tauri :1420 — so dev keeps working). PROD = rlmlocal.com. Never `*`.
fn is_allowed_origin(origin: &str) -> bool {
  origin == "https://rlmlocal.com"
    || origin == "https://www.rlmlocal.com"
    || origin == "https://rlmlocal-site.pages.dev"
    || origin == "http://localhost"
    || origin == "http://127.0.0.1"
    || origin.starts_with("http://localhost:")
    || origin.starts_with("http://127.0.0.1:")
}

/// CORS headers for the bridge. REFLECTS the request Origin only if it's whitelisted (dev localhost OR prod
/// rlmlocal.com) — not `*`. A disallowed/absent origin gets NO Allow-Origin header → the browser blocks the
/// cross-origin read. (X-RLM-Token is allowed for the pairing token.)
fn cors_response(body: &str, status: u16, origin: Option<&str>) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
  let mut resp = tiny_http::Response::from_string(body).with_status_code(status);
  for (k, v) in [
    ("Access-Control-Allow-Methods", "GET, POST, OPTIONS"),
    ("Access-Control-Allow-Headers", "Content-Type, X-RLM-Token"),
    // Private Network Access (Chrome): a PUBLIC https origin (rlmlocal.com) calling a PRIVATE address
    // (127.0.0.1) triggers a PNA preflight that REQUIRES this header, or the browser blocks the request.
    // Dev (localhost→localhost) never needed it; production (rlmlocal.com→127.0.0.1) does. THIS was the
    // "executor won't connect from prod" bug — the probe + every call was silently blocked without it.
    ("Access-Control-Allow-Private-Network", "true"),
    ("Content-Type", "application/json"),
  ] {
    if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
      resp.add_header(h);
    }
  }
  if let Some(o) = origin.filter(|o| is_allowed_origin(o)) {
    if let Ok(h) = tiny_http::Header::from_bytes(b"Access-Control-Allow-Origin", o.as_bytes()) {
      resp.add_header(h);
    }
  }
  resp
}

/// HTTP bridge (Model B). Tauri's native `invoke` only works inside the app window, so an EXTERNAL browser
/// frontend (Vite :5173 in dev, rlmlocal.com in prod) can't reach Rust. This exposes the SAME execution
/// primitives over a localhost HTTP endpoint so the browser frontend can drive the executor. Own thread, bound
/// to 127.0.0.1 only (pt 4). Now compiled into RELEASE too (pt 1), secured by domain CORS (pt 2) + the pairing
/// token (pt 3, enforced in release; debug skips it so the local dev loop stays frictionless).
fn start_http_bridge() {
  {
    std::thread::spawn(|| {
      let server = match tiny_http::Server::http(("127.0.0.1", BRIDGE_PORT)) {
        Ok(s) => s,
        Err(e) => {
          eprintln!("[bridge] could not bind 127.0.0.1:{BRIDGE_PORT} (dev HTTP bridge disabled): {e}");
          return;
        }
      };
      println!("[bridge] dev HTTP bridge on http://127.0.0.1:{BRIDGE_PORT} — external-browser invoke (POST /run_tests · /verify_patch · /apply_patch · /resolve_types · /compute_refactor · /git_cochange · /find_literal_targets · /opencode_*)");
      for mut req in server.incoming_requests() {
        let origin = req.headers().iter().find(|h| h.field.equiv("Origin")).map(|h| h.value.as_str().to_string());
        if *req.method() == tiny_http::Method::Options {
          let _ = req.respond(cors_response("", 200, origin.as_deref())); // CORS preflight
          continue;
        }
        let is_post = *req.method() == tiny_http::Method::Post;
        // PAIRING TOKEN (release only — debug skips so the local dev loop stays frictionless): every POST must
        // carry the correct X-RLM-Token, else 401 before anything runs. CORS stops browser cross-origin reads;
        // the token stops a non-browser client from POSTing to the local port.
        if is_post && !cfg!(debug_assertions) {
          let tok = req.headers().iter().find(|h| h.field.equiv("X-RLM-Token")).map(|h| h.value.as_str().to_string());
          if tok.as_deref() != Some(exec_token()) {
            let _ = req.respond(cors_response(&serde_json::json!({ "error": "unpaired executor — pair it with the code shown on launch" }).to_string(), 401, origin.as_deref()));
            continue;
          }
        }
        let url = req.url().to_string();
        let body = std::io::read_to_string(req.as_reader()).unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
        // Route to the SAME inner fns the native `invoke` commands use — one logic, two transports.
        let result: Result<String, String> = if is_post && url.starts_with("/run_tests") {
          run_tests_inner(s("projectPath")).map(|r| serde_json::to_string(&r).unwrap_or_default())
        } else if is_post && url.starts_with("/verify_patch") {
          let edits = v.get("edits").and_then(|e| serde_json::from_value::<Vec<Edit>>(e.clone()).ok());
          verify_patch_inner(s("projectPath"), s("file").unwrap_or_default(), edit_list(s("search"), s("replace"), edits))
            .map(|r| serde_json::to_string(&r).unwrap_or_default())
        } else if is_post && url.starts_with("/apply_patch") {
          let edits = v.get("edits").and_then(|e| serde_json::from_value::<Vec<Edit>>(e.clone()).ok());
          apply_patch_inner(s("projectPath"), s("file").unwrap_or_default(), edit_list(s("search"), s("replace"), edits), s("message"))
            .map(|r| serde_json::to_string(&r).unwrap_or_default())
        } else if is_post && url.starts_with("/resolve_types") {
          let positions = v.get("positions").and_then(|p| serde_json::from_value::<Vec<Position>>(p.clone()).ok()).unwrap_or_default();
          // the resolver already returns JSON ({ types: [...] }) — pass it through verbatim. `content` = converge overlay.
          resolve_types_inner(s("projectPath"), s("file").unwrap_or_default(), positions, s("content"))
        } else if is_post && url.starts_with("/compute_refactor") {
          let args = v.get("args").cloned().unwrap_or(serde_json::Value::Null);
          // the script returns JSON ({ ok, edits:[...] }) — pass it through verbatim. `content` = converge overlay.
          compute_refactor_inner(s("projectPath"), s("file").unwrap_or_default(), s("kind").unwrap_or_default(), args, s("content"))
        } else if is_post && url.starts_with("/git_cochange") {
          git_cochange_inner(s("projectPath")).map(|pairs| serde_json::json!({ "pairs": pairs }).to_string())
        } else if is_post && url.starts_with("/find_literal_targets") {
          let literals = v.get("literals").and_then(|items| serde_json::from_value::<Vec<String>>(items.clone()).ok()).unwrap_or_default();
          find_literal_targets_inner(s("projectPath"), literals).map(|matches| serde_json::to_string(&matches).unwrap_or_else(|_| "[]".into()))
        } else if is_post && url.starts_with("/opencode_status") {
          Ok(opencode_host::status_json())
        } else if is_post && url.starts_with("/opencode_start") {
          opencode_host::start_json(s("projectPath"))
        } else if is_post && url.starts_with("/opencode_stop") {
          opencode_host::stop_json()
        } else if is_post && url.starts_with("/opencode_ensure") {
          opencode_host::ensure_json(s("projectPath"))
        } else if is_post && url.starts_with("/opencode_task_start") {
          opencode_host::task_start_json(s("projectPath"), s("model"), s("ollamaBase"), v.get("provider").filter(|p| !p.is_null()).cloned())
        } else if is_post && url.starts_with("/opencode_task_collect") {
          opencode_host::task_collect_json(s("taskId"))
        } else if is_post && url.starts_with("/opencode_task_turn") {
          opencode_host::task_turn_json(s("taskId"), s("message"), s("model"), s("ollamaBase"))
        } else if is_post && url.starts_with("/opencode_task_cleanup") {
          opencode_host::task_cleanup_json(s("taskId"))
        } else if is_post && url.starts_with("/opencode_run") {
          opencode_host::run_json(
            s("projectPath"),
            s("message").unwrap_or_default(),
            s("model"),
            s("ollamaBase"),
            v.get("provider").filter(|p| !p.is_null()).cloned(),
          )
        } else {
          Err("not found".to_string())
        };
        let json = result.unwrap_or_else(|e| serde_json::json!({ "error": e }).to_string());
        let _ = req.respond(cors_response(&json, 200, origin.as_deref()));
      }
    });
  }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  // Linux/Wayland: WebKitGTK's DMABUF renderer crashes under some Wayland compositors. Disabling it
  // here ships with the binary, so every installed user gets the fix automatically.
  #[cfg(target_os = "linux")]
  std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");

  tauri::Builder::default()
    .plugin(tauri_plugin_opener::init()) // sandbox fork: one-click Pair now
    .plugin(tauri_plugin_updater::Builder::new().build()) // sandbox fork: GitHub auto-update
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      // AUTO-UPDATE (release only): check GitHub for a newer SIGNED release. Sandbox fork.
      #[cfg(not(debug_assertions))]
      {
        let handle = app.handle().clone();
        tauri::async_runtime::spawn(async move {
          use tauri_plugin_updater::UpdaterExt;
          if let Ok(updater) = handle.updater() {
            if let Ok(Some(update)) = updater.check().await {
              println!("[updater] new version {} available — downloading…", update.version);
              if update.download_and_install(|_, _| {}, || {}).await.is_ok() {
                println!("[updater] installed — restart the executor to apply");
              }
            }
          }
        });
      }
      // Show the pairing code (release enforces X-RLM-Token; debug skips it). The cockpit also reads it via
      // the `get_pairing_code` command — the user pastes it into rlmlocal.com to authorize this executor.
      println!("[bridge] PAIRING CODE: {}  — paste into rlmlocal.com to authorize this executor", exec_token());
      start_http_bridge(); // external-browser bridge on 127.0.0.1:1421 (release + debug; release needs the token)
      Ok(())
    })
    .invoke_handler(tauri::generate_handler![
      run_tests, verify_patch, apply_patch, resolve_types, compute_refactor, git_cochange, find_literal_targets, get_pairing_code,
      opencode_status, opencode_start, opencode_stop, opencode_ensure, opencode_run,
      opencode_task_start, opencode_task_collect, opencode_task_turn, opencode_task_cleanup
    ])
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}

#[cfg(test)]
mod literal_target_tests {
  use super::*;

  #[test]
  fn scans_exact_source_literals_without_a_git_index() {
    let root = std::env::temp_dir().join(format!("rlm-literal-target-{}-{}", std::process::id(), now_ms()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("src")).expect("source directory");
    std::fs::write(root.join("package.json"), "{}\n").expect("manifest");
    std::fs::write(
      root.join("src/stripThinkTags.ts"),
      "export const strip = (s: string) => s.replace(/<think>[\\s\\S]*?<\\/think>/gi, '');\n",
    ).expect("implementation source");
    std::fs::write(root.join("src/other.ts"), "export const other = true;\n").expect("other source");

    let found = find_literal_targets_at_root(&root, &["<think>".into(), "</think>".into()]).expect("literal scan");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, "src/stripThinkTags.ts");
    assert_eq!(found[0].hits, 2);

    let _ = std::fs::remove_dir_all(root);
  }
}

#[cfg(test)]
mod verify_honesty_tests {
  use super::*;

  fn mkroot(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("rlm-verify-honesty-{name}-{}-{}", std::process::id(), now_ms()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root dir");
    root
  }

  #[test]
  fn pytest_runner_prefers_the_project_venv() {
    let root = mkroot("venv");
    std::fs::write(root.join("pytest.ini"), "[pytest]\n").expect("manifest");
    std::fs::create_dir_all(root.join(".venv/bin")).expect("venv bin");
    std::fs::write(root.join(".venv/bin/python"), "#!/bin/sh\n").expect("venv python");

    let (prog, args, label) = detect_runner(&root).expect("runner");
    assert_eq!(label, "python");
    assert!(prog.ends_with(".venv/bin/python"), "prog was {prog}");
    assert_eq!(args, vec!["-m".to_string(), "pytest".to_string()]);

    let _ = std::fs::remove_dir_all(root);
  }

  #[test]
  fn pytest_runner_falls_back_to_path_pytest_without_a_venv() {
    let root = mkroot("novenv");
    std::fs::write(root.join("pytest.ini"), "[pytest]\n").expect("manifest");

    let (prog, _args, label) = detect_runner(&root).expect("runner");
    assert_eq!(label, "python");
    assert_eq!(prog, "pytest");

    let _ = std::fs::remove_dir_all(root);
  }

  #[cfg(unix)]
  #[test]
  fn run_with_timeout_kills_a_hanging_command_instead_of_freezing() {
    let mut cmd = Command::new("sleep");
    cmd.arg("30");
    let start = std::time::Instant::now();
    let err = run_with_timeout(&mut cmd, std::time::Duration::from_millis(300)).expect_err("must time out");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    assert!(start.elapsed() < std::time::Duration::from_secs(5), "kill should be prompt");
  }

  #[cfg(unix)]
  #[test]
  fn run_with_timeout_collects_output_of_a_fast_command() {
    let mut cmd = Command::new("echo");
    cmd.arg("hello");
    let out = run_with_timeout(&mut cmd, std::time::Duration::from_secs(10)).expect("echo runs");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("hello"));
  }

  #[test]
  fn runner_table_preserves_the_old_selection_order() {
    let root = mkroot("order");
    std::fs::write(root.join("package.json"), "{}\n").unwrap();
    std::fs::write(root.join("pytest.ini"), "[pytest]\n").unwrap();
    let (prog, _args, label) = detect_runner(&root).expect("runner");
    assert_eq!(label, "node"); // package.json beats the python manifests — same as the old if-chain
    assert!(prog.ends_with("npm") || prog.ends_with("npm.cmd"));
    let _ = std::fs::remove_dir_all(&root);
  }

  #[test]
  fn code_exts_table_covers_the_previously_drifted_extensions() {
    // the co-change filter used to drop these (LANG-REG): modern JS variants + regex-only languages
    for ext in [".mjs", ".cjs", ".mts", ".cts", ".java", ".rb", ".php", ".cpp", ".hpp", ".scala"] {
      assert!(CODE_EXTS.contains(&ext), "{ext} missing from CODE_EXTS");
    }
    assert_eq!(CODE_EXTS.len(), 27); // identical to ALL_CODE_EXTS on the TS side
  }

  #[test]
  fn parse_check_table_maps_only_supported_extensions() {
    assert!(parse_check_cmd("js").is_some());
    assert!(parse_check_cmd("mjs").is_some());
    assert!(parse_check_cmd("py").is_some());
    assert!(parse_check_cmd("go").is_none());
    assert!(parse_check_cmd("rs").is_none());
  }
}
