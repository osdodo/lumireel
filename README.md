<p align="center">
  <img src="assets/logo.png" alt="LumiReel" width="128" height="128">
</p>

<h1 align="center">LumiReel</h1>

<p align="center">
  Make videos with a coding agent — every frame is code.
</p>

<p align="center">
  <a href="LICENSE"><img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-blue.svg"></a>
  <img alt="Platform: macOS" src="https://img.shields.io/badge/platform-macOS-lightgrey.svg">
  <img alt="Built with Rust" src="https://img.shields.io/badge/built%20with-Rust-orange.svg">
</p>

<p align="center">
  <b>English</b> · <a href="README.zh-CN.md">简体中文</a>
</p>

---

LumiReel is a native desktop app for producing videos together with an AI coding agent. A video is
a small, deterministic HTML program; you describe what you want, the agent writes it, and LumiReel
previews and renders it.

## Features

- **Agent-native workflow** — the agent works in a real PTY inside the project folder, with the `reel`
  CLI on its `PATH` and project instructions (`AGENTS.md` / `CLAUDE.md`) installed for it.
- **Point at what you mean** — select a time range or drag a box over the preview, and a precise
  reference is pasted into the agent's prompt.
- **Session history** — the agent's own past sessions are read from Claude Code, Pi, Codex and
  OpenCode, so a restart or an app quit does not lose the conversation; resume one from the history
  button in the agent panel.
- **Deterministic by design** — a video is a pure function of time, so any frame can be inspected,
  checked and re-rendered exactly.
- **Live preview** — edits on disk are picked up automatically.
- **One-command export** — `reel render` writes an MP4 via `ffmpeg`, including the audio mix.

## How a video works

Each project is a folder containing one HTML page with a single `<canvas>` and a
`window.reel.paint(t)` function:

```js
window.reel.paint = (t) => {
  // Draw the frame at time t (seconds). The same t must always produce the same picture.
};
```

Because frames depend only on `t`, LumiReel can seek, sample and render without playing the video
in real time.

## Requirements

- macOS 12 or later
- [`ffmpeg`](https://ffmpeg.org) on `PATH` (e.g. `brew install ffmpeg`) — required for rendering and some media
- At least one coding agent CLI installed, e.g. [Claude Code](https://docs.anthropic.com/en/docs/claude-code)
- To build from source: a recent stable [Rust](https://rustup.rs) toolchain (edition 2024)

## Installation

### Build the macOS app

```sh
./scripts/bundle-macos.sh
open dist/LumiReel.app
```

The script builds both binaries in release mode for the current architecture and assembles
`dist/LumiReel.app` with its icon. Drag
it into `/Applications` to install. The bundle is ad-hoc signed but not notarized: on another Mac,
open it the first time with right-click → **Open**.

### Run from source

```sh
cargo build --release
./target/release/lumireel
```

## Usage

1. Launch LumiReel and click **New Project**. The project starts from a 3-second template.
2. In the agent terminal, describe the video you want.
3. Watch the preview update as the agent edits. Select a range or region to give it precise feedback.
4. Render the final video with `reel render`.

### The `reel` CLI

`reel` ships with the app and is available inside every agent terminal:

| Command | Description |
|---|---|
| `reel peek` | Contact sheet of the whole video, plus a determinism check |
| `reel peek 4.5` | A single frame at full size |
| `reel peek 3-6` | Twelve frames sampled from a time range |
| `reel render [--fps 30] [-o out.mp4]` | Render the project to MP4 (default `<project>.mp4`) |

## Configuration

| Setting | Default |
|---|---|
| Project library | `~/Movies/lumireel` on macOS (`~/Videos/lumireel` elsewhere) |
| Library override | `LUMIREEL_LIBRARY_DIR` environment variable |
| App settings | `~/Library/Application Support/lumireel/config.json` |
| Interface language | Follows the system language, changeable in Settings (gear icon, top right) |
| Language override | `LUMIREEL_LOCALE=<code>`, one of `en`, `zh-CN`, `zh-TW`, `ja`, `ko`, `es`, `fr`, `de`, `pt-BR`, `ru` |

Translations live in `locales/<code>.yml`. Missing keys fall back to English.

The agent command chosen in the UI is remembered between launches.

## Development

```sh
cargo test                      # video format, prompt block and ffmpeg mix
cargo run --bin reel -- help    # CLI usage
RUST_LOG=info cargo run         # run with logging
```

## License

Released under the [MIT License](LICENSE).
