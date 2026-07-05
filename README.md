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
- Useful for multi-directory work, such as coordinating frontend/backend changes or two related microservices at once.
- `plan` mode for read-only work and `build` mode for file-changing work.
- Streaming assistant output, reasoning, and tool results.
- Runtime-configured MCP tool servers with explicit transport selection.
- Configurable OpenAI-compatible, Anthropic, and MiniMax-style providers.
- Project switching, provider settings, and light/dark theme support.

## Why Arachne

Arachne is built for work that does not fit cleanly into one terminal, one chat, or one repository. Each session can keep its own context while still being connected to related sessions when useful.

Examples:

- Work on two microservices at once: keep one session focused on the API contract and service A, another on service B, then connect them so each can inspect the other side when needed.
- Work on frontend and backend in separate directories: keep UI changes, API changes, migrations, and test runs scoped to the right project while preserving a shared understanding of the feature.
- Investigate a bug across repos: use one session to trace the caller and another to inspect the downstream service without constantly reloading context.
- Separate planning from implementation: use a read-only planning session to map the change, then hand focused implementation tasks to build-mode sessions.

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

## MCP Servers

MCP servers are runtime configuration, not prompt text. Tools discovered from MCP servers are exposed to the agent as namespaced tools like `mcp__server__tool`.

Supported transports are explicit:

- `stdio`: start a local command and speak MCP over stdin/stdout.
- `streamable_http`: use an MCP Streamable HTTP endpoint.
- `sse`: use the legacy MCP SSE endpoint and its server-provided POST endpoint.
- `polling_http`: use plain, non-streaming JSON-RPC over HTTP POST.

HTTP and SSE URLs may point at localhost or remote hosts. `polling_http` is intentionally limited and is not a fallback for Streamable HTTP; polling servers should also expose a health endpoint so readiness checks can verify the service before tool discovery.

MCP configuration can come from global config, project `.arachne/config.json`, or app settings. Use MCP when a project needs runtime-owned access to external tools, internal services, local helper commands, or organization-specific context providers without injecting that setup into the model prompt.

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
