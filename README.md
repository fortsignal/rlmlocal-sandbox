<div align="center">

# RLMlocal Sandbox

**The local execution sandbox for [RLMlocal](https://rlmlocal.com).**

A tiny desktop helper that runs your tests and applies **approved** code changes on your own machine.
The thinking happens in your browser at [rlmlocal.com](https://rlmlocal.com) — this app is just the
deterministic *hands*.

`local-first` · `human-approved` · `open & inspectable`

</div>

---

## What it is

RLMlocal is split in two:

| | Where it runs | What it does |
|---|---|---|
| 🧠 **The brain** | your browser, [rlmlocal.com](https://rlmlocal.com) | reads your code, plans refactors, reasons over the graph |
| ✋ **The hands** *(this repo)* | your machine | runs tests, verifies changes in a throwaway shadow, applies the diffs you approve |

They talk over a strictly-local HTTP bridge on `127.0.0.1:1421`. **The brain proposes; you approve every
change; only then do the hands write a single byte.** Nothing lands without your explicit OK.

> This repo is **only the sandbox executor**. The intelligence lives at rlmlocal.com — it's never bundled
> here. What you see is exactly what runs on your machine: a small Rust process and a one-screen window.

## Download

Grab your build from the [**Releases**](../../releases) page:

### 🍎 macOS — Intel + Apple Silicon
- `Rlmlocal.Sandbox_..._universal.dmg` — installer

### 🪟 Windows
- `Rlmlocal.Sandbox_..._x64-setup.exe` — one-click installer (recommended)
- `Rlmlocal.Sandbox_..._x64_en-US.msi` — MSI alternative

### 🐧 Linux
- `Rlmlocal.Sandbox_..._amd64.AppImage` — portable, runs on any distro (no install)
- `Rlmlocal.Sandbox_..._amd64.deb` — Debian / Ubuntu
- `Rlmlocal.Sandbox-..._x86_64.rpm` — Fedora / RHEL

> **Unsigned beta:** your OS will warn on first launch (not code-signed yet).
> macOS: right-click → **Open** → **Open** · Windows: **More info** → **Run anyway** · Linux: just install/run it.

## Pair it (one time)

1. **Launch** RLMlocal Sandbox — a small window opens showing **🟢 running**.
2. Click **"Pair now"** — it opens [rlmlocal.com](https://rlmlocal.com) in your browser and connects automatically.
3. Done — your browser and this executor are now bound. Keep the window open while you work.

*(Prefer manual? The window also shows a pairing code you can paste at rlmlocal.com.)*
The pairing token is required on **every** request, and the bridge only accepts calls from `rlmlocal.com`.

## Security

- 🔒 **Localhost only** — the bridge binds to `127.0.0.1`, never `0.0.0.0`. Nothing off your machine can reach it.
- 🎫 **Paired** — release builds require your pairing token on every request.
- 🌐 **Origin-locked** — CORS reflects **only** `rlmlocal.com` (+ `www` and the staging Pages domain), never `*`.
- ✅ **Verify-then-approve** — every change is run against your tests in a throwaway shadow copy first, and
  the diff is shown to you. You approve; then it applies. No silent writes.
- 🔍 **Inspectable** — the entire executor is in this repo (`src-tauri/`). It's a small, readable Rust program.

## Build from source

Requires [Rust](https://rustup.rs) + the [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/)
for your platform.

```sh
cd src-tauri
cargo tauri build      # production (release) build — pairing token enforced
```

The release binary + installers land in `src-tauri/target/release/bundle/`.

## How it's structured

```
rlmlocal-sandbox/
├─ src-tauri/      the Rust executor (bridge + run/verify/apply commands)
├─ ui/             the one-screen pairing window
├─ scripts/        the TS-compiler type resolver the executor runs
└─ .github/        the release workflow (mac / windows / linux)
```

---

<div align="center">
<sub>Part of <a href="https://rlmlocal.com">RLMlocal</a> — a local-first cognitive runtime. The brain stays yours.</sub>
</div>
