<!-- Edit this BEFORE each release (before `release-executor.sh <ver>` + tag push). The release workflow
     prepends this file's contents to the GitHub release body, above the download/setup section.
     Keep it user-facing (what changed, in plain terms) — no internal symbol names. -->

## ✨ What's new in v0.1.6

- **Honest "0 tests" verdicts** — verification now reports when a test suite never actually ran (no test evidence) instead of showing a clean GREEN. On those projects the app says "0 tests detected (types/lint only)" — a suite that can't start can no longer make a bad change look verified.
- **Shadow cleanup** — verification worktrees now clean up their own stale registrations (crashed or interrupted runs used to accumulate them).
- Existing structure path (verify → Approve → land) unchanged.

*Existing installs auto-update on next launch when this release is published. Still an unsigned beta.*
