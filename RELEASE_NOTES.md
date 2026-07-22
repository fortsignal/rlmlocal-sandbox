<!-- Edit this BEFORE each release (before `release-executor.sh <ver>` + tag push). The release workflow
     prepends this file's contents to the GitHub release body, above the download/setup section.
     Keep it user-facing (what changed, in plain terms) — no internal symbol names. -->

## ✨ What's new in v0.1.5

- **Graph coder hands** — local OpenCode task host so the CODE dock can run multi-step coding in a shadow worktree (still requires your Approve before anything lands).
- **Co-change signals** — the executor can report which files historically change together (for planning / graph), now covering modern JS variants and more languages.
- **Literal source scan** — exact on-disk search helper for implementation planning (read-only).
- **Python verify, fixed** — refactors on Python projects now verify against YOUR project's own environment (your venv is used instead of a stray global pytest), and a stuck or watch-mode test run can no longer freeze verification — it now fails honestly instead of hanging or, worse, passing without running.
- Existing structure path (verify → Approve → land) unchanged.

*Existing installs auto-update on next launch when this release is published. Still an unsigned beta.*
