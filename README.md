# Arachne

<p align="center">
  <img src="src-tauri/icons/node-web.svg" alt="Arachne node web logo" width="128" height="128" />
</p>

Arachne is a desktop AI coding agent built with Tauri, Rust, React, and TypeScript.
It lets you run multiple coding sessions on a canvas, connect sessions so they can share context, and work with LLM tools against local project files.

## Features

- Visual session canvas with draggable nodes and connections.
- Multiple persistent chats per session.
- Connected sessions can read/search peer project context.
- `plan` mode for read-only work and `build` mode for file-changing work.
- Streaming assistant output, reasoning, and tool results.
- Configurable OpenAI-compatible, Anthropic, and MiniMax-style providers.
- Project switching, provider settings, and light/dark theme support.

## Requirements

- Node.js and npm.
- Rust toolchain.
- Tauri prerequisites for your OS.
- On Windows: install Visual Studio Build Tools with the **Desktop development with C++** workload.

## Setup

```bash
npm install
```

## Run

```bash
npm run tauri:dev
```

For frontend-only development:

```bash
npm run dev
```

## Build

```bash
npm run build
npm run tauri:prod
```

## Providers

Providers are configured from the app settings. Supported protocols include:

- OpenAI-compatible `/v1/chat/completions` providers.
- Anthropic Messages API providers.
- MiniMax-compatible providers.

Optional debug logging:

```bash
RUST_LOG=arachne=debug,tauri=info npm run tauri:dev
```

## Project Layout

- `desktop/` - React UI, session canvas, stores, and components.
- `src-tauri/` - Tauri shell, commands, app services, and IPC wiring.
- `agents/` - Rust agent runtime, sessions, tools, providers, routing, and persistence.
- `src-tauri/icons/` - App icons and logo assets.

## Useful Commands

```bash
cargo fmt
cargo check
cargo test -p arachne-agents
npm run build
```

