# Arachne

```
                         █████╗ ██████╗  █████╗  ██████╗██╗  ██╗███╗   ██╗███████╗
                        ██╔══██╗██╔══██╗██╔══██╗██╔════╝██║  ██║████╗  ██║██╔════╝
                        ███████║██████╔╝███████║██║     ███████║██╔██╗ ██║█████╗  
                        ██╔══██║██╔══██╗██╔══██║██║     ██╔══██║██║╚██╗██║██╔══╝  
                        ██║  ██║██║  ██║██║  ██║╚██████╗██║  ██║██║ ╚████║███████╗
                        ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═╝ ╚═════╝╚═╝  ╚═╝╚═╝  ╚═══╝╚══════╝
```

> _A network-weaving AI coding agent — many threads, one web._

Arachne is an open-source AI coding agent that lets you spin up multiple LLM
sessions, lay them out on a visual canvas as a graph, and have them share
context with each other in real time. Sessions can `task` out subtasks to
children and `interject` snippets of local context (file contents, search hits,
shell output) directly into the active conversation. Built with Rust and
Tauri.

An arachnid doesn't think in a straight line — it spins a web. Each strand is
its own thread of reasoning, anchored at a point but connected to every other
strand. Arachne applies the same idea to coding sessions:

- **A session is a strand** — an isolated conversation with a chosen
  provider/model that produces its own output.
- **The canvas is the web** — sessions are nodes you can drag, connect, and
  group. `parent_session_id` on every strand records the genealogy.
- **The runner is the spinner** — the central loop that pulls LLM events,
  weaves them into the conversation file, and dispatches tool calls.
- **Sub-agents are forked threads** — a `task` spawns a child session whose
  results feed back into the parent's conversation.
- **Peers are sibling strands** — connected sessions can target each other
  directly by passing the peer's `peer_session_id` to read/glob/grep/plan
  tools.


## Features

- **Canvas-based session management** — Visual graph (React Flow) of AI
  coding sessions with drag-and-drop nodes and edges
- **Peer-to-peer session context** — A `parent_session_id` link records the
  genealogy; child sessions feed results back into the parent's
  conversation. Connected sibling sessions can be addressed by passing
  `peer_session_id` to read/glob/grep/plan tools
- **Snippet interjection** — Pull a file range, a search hit, or shell
  output into the active turn without rewriting the prompt
- **Real-time streaming** — Live message updates during agent execution,
  with structured `LlmEvent` deltas (text, reasoning, tool calls, finish)
- **Plan / Build permission modes** — Read-only `plan` mode denies
  mutations; `build` mode allows them.
- **Multiple LLM providers** — Pluggable OpenAI-compatible and Anthropic
  transports
- **Project management** — Create and switch between projects, each with
  its own directory, tech-stack detection, and session set
- **Dark / Light theme** — Toggle in settings

## Providers

Arachne ships with built-in support for OpenAI-compatible, Anthropic, and
MiniMax Token Plan providers. Anything that exposes an
`/v1/chat/completions` (OpenAI-compatible), Anthropic Messages, or
MiniMax-compatible endpoint can be configured as a custom provider.

## Architecture

**Frontend** — React + TypeScript + Vite + Zustand + React Flow
(`desktop/`)

**Backend** — Rust + Tauri (`src-tauri/`) + `arachne-agents` crate
(`agents/`)

The Tauri shell owns windows, IPC, and the `arachne` Tauri command surface.
All LLM, tool, and session logic lives in the `arachne-agents` crate and is
exposed to the frontend as `#[tauri::command]` functions in
`src-tauri/src/commands/`.

## Development

```bash
# Install dependencies
npm install

# Run frontend dev server
npm run dev

# Run Tauri dev (from project root)
npm run tauri:dev
```

Set `RUST_LOG=arachne=debug,tauri=info` to see the per-event LLM stream
log (every `LlmEvent` flowing through the runner) and the persistence
log emitted at the end of each turn.

## License

TBD.
