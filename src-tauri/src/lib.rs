use std::path::{Path, PathBuf};
use std::process::Command;

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
    match Command::new(npm).args(["run", "lint", "--silent"]).current_dir(root).envs(env.iter().map(|(k, v)| (k, v))).output() {
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
    match Command::new(npx).args(["tsc", "--noEmit"]).current_dir(root).envs(env.iter().map(|(k, v)| (k, v))).output() {
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

/// AGNOSTIC runner detection — map the project's manifest to its test command.
fn detect_runner(root: &Path) -> Result<(String, Vec<String>, &'static str), String> {
  let has = |f: &str| root.join(f).exists();
  if has("package.json") {
    let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
    Ok((npm.to_string(), vec!["test".into()], "node"))
  } else if has("Cargo.toml") {
    Ok(("cargo".into(), vec!["test".into()], "rust"))
  } else if has("go.mod") {
    Ok(("go".into(), vec!["test".into(), "./...".into()], "go"))
  } else if has("pyproject.toml")
    || has("setup.py")
    || has("setup.cfg")
    || has("pytest.ini")
    || has("requirements.txt")
    || has("conftest.py")
  {
    Ok(("pytest".into(), vec![], "python"))
  } else if has("Makefile") {
    Ok(("make".into(), vec!["test".into()], "make"))
  } else {
    Err("couldn't detect a test runner for this project".into())
  }
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

/// Run the project's suite once, runner auto-detected, env loaded.
/// Returns (runner, exit_code, failed_count, combined-output).
fn run_suite(root: &Path) -> Result<(String, i32, i64, String), String> {
  let (prog, args, label) = detect_runner(root)?;
  let env = load_project_env(root);
  let output = Command::new(&prog)
    .args(&args)
    .current_dir(root)
    .envs(env)
    .output()
    .map_err(|e| format!("failed to spawn {prog}: {e}"))?;
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

fn now_ms() -> u128 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_millis())
    .unwrap_or(0)
}

/// Create a SHADOW of the project via `git worktree` so verification NEVER touches the real source.
/// node_modules is symlinked (copying GBs is absurd); gitignored env files are copied in so the shadow
/// runs with the same env. The shadow checks out HEAD — the real working tree is read-only, untouched.
fn make_shadow(real_root: &Path) -> Result<PathBuf, String> {
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
  if let Ok(diff) = Command::new("git").args(["-C", real, "diff", "HEAD", "--binary"]).output() {
    if diff.status.success() && !diff.stdout.is_empty() {
      use std::io::Write;
      if let Ok(mut child) = Command::new("git")
        .args(["-C", shadow_s, "apply", "--whitespace=nowarn"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
      {
        if let Some(mut stdin) = child.stdin.take() { let _ = stdin.write_all(&diff.stdout); }
        let _ = child.wait();
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
fn remove_shadow(real_root: &Path, shadow: &Path) {
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
fn parse_check(root: &Path, file: &str) -> Option<String> {
  let ext = Path::new(file).extension().and_then(|e| e.to_str()).unwrap_or("");
  let (prog, args): (&str, Vec<&str>) = match ext {
    "js" | "mjs" | "cjs" | "jsx" => ("node", vec!["--check", file]),
    "py" => ("python3", vec!["-m", "py_compile", file]),
    _ => return None,
  };
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
  Ok(ApplyResult { applied: true, file })
}

#[tauri::command]
fn apply_patch(
  project_path: Option<String>,
  file: String,
  search: Option<String>,
  replace: Option<String>,
  edits: Option<Vec<Edit>>,
) -> Result<ApplyResult, String> {
  apply_patch_inner(project_path, file, edit_list(search, replace, edits))
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
fn resolve_types_inner(project_path: Option<String>, file: String, positions: Vec<Position>) -> Result<String, String> {
  let root = project_path.ok_or_else(|| "resolve_types: no project path".to_string())?;
  const SCRIPT: &str = include_str!("../../scripts/resolveExtractTypes.cjs");
  let script_path = std::env::temp_dir().join("rlm-resolve-extract-types.cjs");
  std::fs::write(&script_path, SCRIPT).map_err(|e| format!("write resolver: {e}"))?;
  let input = serde_json::json!({ "root": root, "file": file, "positions": positions }).to_string();
  let node = if cfg!(windows) { "node.exe" } else { "node" };
  let mut child = Command::new(node)
    .arg(&script_path)
    .current_dir(&root)
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
    return Err(format!("resolver no output: {err}"));
  }
  Ok(s)
}

#[tauri::command]
fn resolve_types(project_path: Option<String>, file: String, positions: Vec<Position>) -> Result<serde_json::Value, String> {
  let s = resolve_types_inner(project_path, file, positions)?;
  serde_json::from_str(&s).map_err(|e| format!("parse resolver output: {e}"))
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
    ("Access-Control-Allow-Methods", "POST, OPTIONS"),
    ("Access-Control-Allow-Headers", "Content-Type, X-RLM-Token"),
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
      println!("[bridge] dev HTTP bridge on http://127.0.0.1:{BRIDGE_PORT} — external-browser invoke (POST /run_tests · /verify_patch · /apply_patch · /resolve_types)");
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
          apply_patch_inner(s("projectPath"), s("file").unwrap_or_default(), edit_list(s("search"), s("replace"), edits))
            .map(|r| serde_json::to_string(&r).unwrap_or_default())
        } else if is_post && url.starts_with("/resolve_types") {
          let positions = v.get("positions").and_then(|p| serde_json::from_value::<Vec<Position>>(p.clone()).ok()).unwrap_or_default();
          // the resolver already returns JSON ({ types: [...] }) — pass it through verbatim.
          resolve_types_inner(s("projectPath"), s("file").unwrap_or_default(), positions)
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
    .setup(|app| {
      if cfg!(debug_assertions) {
        app.handle().plugin(
          tauri_plugin_log::Builder::default()
            .level(log::LevelFilter::Info)
            .build(),
        )?;
      }
      // Show the pairing code (release enforces X-RLM-Token; debug skips it). The cockpit also reads it via
      // the `get_pairing_code` command — the user pastes it into rlmlocal.com to authorize this executor.
      println!("[bridge] PAIRING CODE: {}  — paste into rlmlocal.com to authorize this executor", exec_token());
      start_http_bridge(); // external-browser bridge on 127.0.0.1:1421 (release + debug; release needs the token)
      Ok(())
    })
    .invoke_handler(tauri::generate_handler![run_tests, verify_patch, apply_patch, resolve_types, get_pairing_code])
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}
