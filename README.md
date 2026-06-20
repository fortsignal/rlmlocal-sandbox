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

## Download & install

Grab the build for your OS from the [**Releases**](../../releases) page:

| OS | File | First-launch (unsigned beta) |
|---|---|---|
| 🍎 **macOS** | `.dmg` | Right-click the app → **Open** → **Open** |
| 🪟 **Windows** | `.msi` / `.exe` | **More info** → **Run anyway** |
| 🐧 **Linux** | `.AppImage` / `.deb` / `.rpm` | `chmod +x` the AppImage, or install the package |

> **Unsigned beta:** your OS will warn on first launch because the build isn't code-signed yet. The steps
> above get you past it. (Signing is on the roadmap.)

## Pair it (one time)

1. **Launch** RLMlocal Sandbox — a small window opens showing **🟢 running** and a **pairing code**.
2. Open **[rlmlocal.com](https://rlmlocal.com)** and paste the code when prompted
   (or go straight to `rlmlocal.com/?pair=<code>`).
3. Done — your browser and this executor are now bound. Keep the window open while you work.

The pairing code is required on **every** request, and the bridge only accepts calls from `rlmlocal.com`.

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
