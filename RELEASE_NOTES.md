<!-- Edit this BEFORE each release (before `release-executor.sh <ver>` + tag push). The release workflow
     prepends this file's contents to the GitHub release body, above the download/setup section.
     Keep it user-facing (what changed, in plain terms) — no internal symbol names. -->

## ✨ What's new in v0.1.4

- **Reference-aware refactoring** — rename, decouple, and extract now run through your project's own TypeScript compiler, so every reference updates correctly, including across files.
- **Auto-commit on land** — each change you approve is committed to git automatically: clean, auditable history, and your working tree stays current.
- **More reliable verification** — fixed a shadow-verify drift that could report "search not found" right after a change landed.
- **Smoother `/converge`** (autonomous decompose) — chained refactors (decouple → move → rename) now verify and apply correctly in sequence.

*Existing installs auto-update to this version on next launch — no re-download needed. Still an unsigned beta; same one-time setup below.*
