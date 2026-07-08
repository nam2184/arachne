# Arachne

<p align="center">
  <img src="src-tauri/icons/node-web.svg" alt="Arachne node web logo" width="128" height="128" />
</p>

Arachne is a desktop AI coding agent built with Tauri, Rust, React, and TypeScript.
It lets you run multiple coding sessions on a canvas, connect sessions so they can share context, and work with LLM tools against local project files.

Arachne is built for technical users who want a coding agent that adapts to their workflow rather than one that hides its seams. Every tool's inputs and outputs are explicit and configurable — JSON-Schema-typed arguments, runtime-side validation, structured metadata returned alongside text — so you can wire it into shell pipelines, scripts, and editor integrations without scraping chat output.

## Features

- Visual session canvas with draggable nodes and connections.
- Multiple persistent chats per session.
- Connected sessions can read/search peer project context.
- Useful for multi-directory work, such as coordinating frontend/backend changes or two related microservices at once.
- `plan` mode for read-only work and `build` mode for file-changing work.
- Streaming assistant output, reasoning, and tool results.
- Runtime-configured MCP tool servers with explicit transport selection.
- Real LSP-style static analysis via tree-sitter (`lsp` tool) — symbols, imports, exports, parse-level diagnostics for 306 languages.
- Plugin ecosystem for additional analysis tools: bring your own binary, MCP server, or Rust crate and expose it as a first-class agent tool (`ghidra` ships as the reference example — wraps any MCP-compliant Ghidra server behind a curated action API).
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

## Tools & Plugins

Tools are the agent's hands. Arachne ships with a curated set of built-ins and supports three ways to add more — pick whichever fits your source.

**Built-in tools.** `read`, `grep`, `glob`, `edit`, `apply_patch`, `write`, `shell`, `task`, `skill`, `todo`, `question`, `webfetch`, `websearch`, `lsp`, `plan`. Each one:

- Declares its arguments as a JSON Schema the model sees verbatim — type, enum, min/max bounds, description.
- Validates against the schema at call time and rejects calls with missing required fields or out-of-range values.
- Returns `ToolResult { success, output, error?, metadata? }`. `metadata` is structured data (parsed JSON from the underlying tool) so callers can drive downstream tools from a previous result without re-parsing text.
- Is sandboxed by `PermissionMode` (`plan` = read-only, `build` = full).

**Static analysis: the `lsp` tool.** LSP-style code intelligence without a real language server. Powered by `tree-sitter-language-pack` (306 grammars, dynamic-loaded shared libraries). Actions: `document` (full s-expr or JSON tree), `symbols`, `diagnostics`, `workspace` (bounded project overview). Diagnostics are parse-level syntax signals, not compiler or real LSP diagnostics — use it for cheap pre-checks before reaching for a heavier tool.

**Reverse engineering: the `ghidra` tool.** Wraps any MCP-compliant Ghidra server behind a curated action API. The user configures a Ghidra server in `[mcp.servers.ghidra]`; the agent calls `ghidra` with high-level actions like `decompile`, `disasm`, `functions`, `xrefs_to`. `ghidra` is the reference example for the third-party tool pattern: pick a JSON-RPC-over-stdio MCP server, write a 600-line Rust file with an action enum and a name → MCP-tool mapping, wire it into the dispatch table. That's it.

**Three ways to add a new tool:**

1. **MCP server** (recommended for external services). Write a server that speaks MCP 2024-11-05 over stdio, HTTP, or SSE. Add an entry to `[mcp.servers]` in your config. Every tool the server exposes becomes available as `mcp__<server>__<tool>`. The agent runtime handles connection lifecycle, timeouts, retries, and tool discovery.
2. **Rust crate in this workspace.** Drop a module under `agents/src/tools/`, implement `run(call, ctx) -> ToolResult` (or `run_with_context` / `run_async` variants), register in the dispatch table, add a `ToolDefinition` to `default_tool_definitions`. The whole round trip — schema, dispatch, tests, permission action — fits in one file. See `agents/src/tools/ghidra.rs` for the full template.
3. **Wrapping an existing CLI.** For ad-hoc local tools, the `shell` tool already exists. For something you want as a first-class agent tool with structured arguments and metadata, write a thin Rust wrapper (like `ghidra`) that calls out to the CLI and translates its text output into structured `metadata`.

The plugin pattern is deliberate: every tool is a regular Rust file, every argument is a JSON Schema, every call goes through the same dispatcher. No magic, no DSL, no required runtime registry update — just `pub mod foo;` and a match arm.

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
