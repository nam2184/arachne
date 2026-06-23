# Plan — Project-scoped LoopCards

## Goal
Add a first-class `Loop` entity, persisted in SQLite, surfaced as a React Flow `LoopNode` on `SessionCanvas`. Loops are owned by a project, hold multiple goals, track a user-set time budget and soft token caps, and feed synthetic turns into the existing chat composer (UI-managed only for v1).

## Data model

Two tables, both created in `agents/src/database/connection.rs::init` with additive `ALTER` fallbacks for existing databases:

- `loops`
  - `id TEXT PRIMARY KEY`
  - `project_id TEXT NOT NULL` → `projects(id)`
  - `kind TEXT NOT NULL CHECK (kind IN ('goal', 'optimisation'))`
  - `title TEXT NOT NULL`
  - `system_prompt TEXT` (nullable; optimisation loops ship a default, goal loops usually blank)
  - `status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','paused','completed','aborted'))`
  - `time_budget_seconds INTEGER NOT NULL DEFAULT 0` (0 = unlimited)
  - `elapsed_seconds INTEGER NOT NULL DEFAULT 0`
  - `started_at TEXT` (nullable, RFC3339)
  - `last_resumed_at TEXT` (nullable)
  - `paused_at TEXT` (nullable)
  - `token_budget_input INTEGER NOT NULL DEFAULT 0`
  - `token_budget_total INTEGER NOT NULL DEFAULT 0`
  - `tokens_input INTEGER NOT NULL DEFAULT 0`
  - `tokens_output INTEGER NOT NULL DEFAULT 0`
  - `tokens_total INTEGER NOT NULL DEFAULT 0`
  - `current_goal_id TEXT` (nullable → goals(id))
  - `position_x REAL`, `position_y REAL` (canvas coords)
  - `created_at TEXT NOT NULL`

- `loop_goals`
  - `id TEXT PRIMARY KEY`
  - `loop_id TEXT NOT NULL` → `loops(id)` ON DELETE CASCADE
  - `text TEXT NOT NULL`
  - `status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','in_progress','done','skipped'))`
  - `order_index INTEGER NOT NULL`
  - `created_at TEXT NOT NULL`
  - `completed_at TEXT` (nullable)

Index `idx_loops_project`, `idx_loop_goals_loop`.

## Rust layer

New files:
- `agents/src/domain.rs` additions: `LoopKind { Goal, Optimisation }`, `LoopStatus`, `GoalStatus`, structs `Loop`, `LoopGoal`. Re-export from `agents/src/lib.rs`.
- `agents/src/sessions/loops.rs` (or `agents/src/loops.rs`):
  - `LoopService { db_path }` mirroring `SessionService`.
  - Methods: `create_loop(project_id, kind, title, system_prompt?, time_budget_seconds, token_budget_input, token_budget_total, position_x, position_y)`, `list_loops_by_project(project_id)`, `get_loop(id)`, `update_loop(...)`, `delete_loop(id)`, `start_loop / pause_loop / resume_loop / complete_loop / abort_loop` (manage the started/elapsed timestamps), `tick_elapsed(id, seconds)` (called by UI tickers), `record_token_usage(id, input_delta, output_delta)` (called by UI), `add_goal(loop_id, text)`, `update_goal(id, text, status)`, `delete_goal(id)`, `set_current_goal(loop_id, goal_id)`, `reorder_goal(goal_id, new_index)`.
  - Optimisation defaults: when `kind = Optimisation` and `system_prompt` is empty, substitute a default constant ("You are an optimiser loop. Inspect the latest assistant output, identify one concrete improvement, and propose the smallest change that moves the stated metric. Do not restate what already works."). Goal-loop defaults are left empty.
- `agents/src/database/repositories.rs`: add `LoopRepository` (insert/find/list/update/delete for loops, plus a tiny set of `loop_goal_*` queries — kept in the same file to match existing pattern).
- `agents/src/database/mod.rs`: re-export `LoopRepository`.
- `src-tauri/src/services/mod.rs` + new `src-tauri/src/services/loop_service.rs`:
  - `pub use arachne_agents::sessions::loops::LoopService as LoopService;` and re-export `Loop`, `LoopGoal`, `LoopKind`, `LoopStatus`, `GoalStatus`.
- `src-tauri/src/commands/mod.rs` + new `src-tauri/src/commands/loop_commands.rs`:
  - `init_loops(project_id) -> { loops: Vec<Loop>, goals: Vec<LoopGoal> }` (returns one project’s worth, joined to goals).
  - `create_loop(project_id, kind, title, time_budget_seconds, token_budget_input, token_budget_total, position_x, position_y) -> id`.
  - `update_loop_position(id, x, y)`.
  - `update_loop(id, fields)`.
  - `delete_loop(id)`.
  - `set_loop_status(id, status)` (active/paused/completed/aborted + auto-manages started_at/paused_at/elapsed).
  - `tick_loop_elapsed(id, seconds)` (UI heartbeat; caps at time_budget_seconds).
  - `record_loop_token_usage(id, input_tokens, output_tokens)`.
  - `add_loop_goal(loop_id, text) -> LoopGoal`.
  - `update_loop_goal(id, text?, status?)`.
  - `delete_loop_goal(id)`.
  - `reorder_loop_goal(id, new_index)`.
  - `set_loop_current_goal(loop_id, goal_id | null)`.
- `src-tauri/src/main.rs`: register `LoopService` in state and `loop_commands::*` in the `tauri::generate_handler!` list. Use the same DB path as `SessionService`.

## Frontend layer

- `src/features/loops/loopStore.ts` (Zustand, mirrors `sessionStore`):
  - State: `loops: Map<string, Loop>`, `goals: Map<string, LoopGoal[]>` keyed by loop id.
  - Actions map 1:1 to the Tauri commands above.
- `src/components/loops/LoopNode.tsx`:
  - React Flow `Node<LoopNodeData>` matching the `SessionNode` visual style (rounded-none borders, monospace, dark theme tokens). Renders:
    - header: title + kind badge (`GOAL` / `OPTIMISATION`) + status badge.
    - time row: elapsed / budget (mm:ss of mm:ss; "—" when budget = 0).
    - token row: in / out / total vs caps.
    - current goal chip.
    - expandable goals list with add input and per-row edit/delete/reorder.
    - action footer: Pause/Resume, Complete, Abort.
  - Handles: open settings popover, drag (already provided by React Flow `position`), connect (a handle that emits `onConnect` to session nodes, but v1 just stores the edges; runtime wiring is deferred).
- `src/components/loops/LoopsToolbar.tsx`: small toolbar at the top of `SessionCanvas` with an `Add loop` button that opens a `NewLoopDialog` (kind radio, title, time/tokens).
- `src/components/loops/NewLoopDialog.tsx`: minimal Radix/Tailwind modal; reuses existing button/input primitives.
- `src/components/loops/LoopTickers.tsx` (hook-style helper): `useLoopTickers(loopIds)` runs a 1s `setInterval` while status = active and calls `tick_loop_elapsed`. Pauses when window hidden.
- Add `LoopNode` to `SessionCanvas.tsx` via `nodeTypes`, loading loops from `loopStore.initialize(projectId)`. Position is persisted via `update_loop_position` on `onNodeDragStop`.
- Re-export the new pieces from `src/components/sessions/index.ts` (or a new `src/components/loops/index.ts` to avoid scope creep).

## Token / time UI behaviour (v1, soft caps)

- Token numbers are user-entered soft caps (`token_budget_input`, `token_budget_total`). Card shows real vs cap. No hard stop in this pass.
- A separate `record_loop_token_usage` command exists for future use; today it’s only wired when the user clicks “Record sample” or when a synthetic turn dispatches (see below). Hooks for `LlmEvent::Usage` aggregation are left out as you noted, but the storage columns and Tauri command are in place so wiring it later is a one-line subscribe in `agent_service`.
- Time tracker: 1-second ticker updates `elapsed_seconds` and clamps at `time_budget_seconds`. When clamped, status auto-flips to `completed` and a `tracing::info!` fires.

## Synthetic-turn integration (UI-managed)

`SessionChat` already has an in-memory `mode` and an `onSendMessage` callback. To make loops actually drive prompts without a backend runner:

- `LoopNode` "Run current goal" button calls a new helper `runGoalTurn(loopId, goalId)` in `loopStore`.
- That helper:
  1. Builds the prompt string from `loop.system_prompt` (optimisation defaults applied) + goal text.
  2. Locates the active session for that project (use `activeSessionId` from `sessionStore`, fallback to first).
  3. Calls the existing `invoke<string>("send_message", { sessionId, message, mode: "build" })` so the chat composer path is reused.
  4. Records a placeholder token delta (0 today) and bumps elapsed via the ticker.

This keeps the v1 "UI-managed loop only" choice clean and reuses the proven `SessionChat` path. A real driver becomes a drop-in later.

## Files touched (summary)

- `agents/src/database/connection.rs` — new tables + indexes + idempotent guards.
- `agents/src/database/repositories.rs` — `LoopRepository`.
- `agents/src/database/mod.rs` — export.
- `agents/src/domain.rs` — `Loop`, `LoopGoal`, enums.
- `agents/src/lib.rs` — re-export new domain items.
- `agents/src/sessions/loops.rs` (new) — `LoopService`.
- `src-tauri/src/services/loop_service.rs` (new) — façade.
- `src-tauri/src/services/mod.rs` — register service.
- `src-tauri/src/commands/loop_commands.rs` (new) — Tauri commands.
- `src-tauri/src/commands/mod.rs` — register.
- `src-tauri/src/main.rs` — wire state + handlers.
- `src/features/loops/loopStore.ts` (new) — Zustand store.
- `src/components/loops/LoopNode.tsx` (new) — canvas node.
- `src/components/loops/LoopsToolbar.tsx` (new).
- `src/components/loops/NewLoopDialog.tsx` (new).
- `src/components/loops/LoopTickers.tsx` (new) — ticker hook.
- `src/components/loops/index.ts` (new) — barrel export.
- `src/components/sessions/SessionCanvas.tsx` — render `LoopNode`, persist positions, dispatch synthetic turns.
- `src/pages/sessions/SessionWorkspace.tsx` — initialize `loopStore` with current project id.

## Open trade-offs / notes

- Persisting `position_x/y` on every drag is chatty; debounce to ~150 ms in `SessionCanvas`.
- Token totals on the loop are *soft* per scope. Columns + a recording command are in place so hooking real `LlmEvent::Usage` later is a one-file change in `agent_service.rs`.
- `system_prompt` for optimisation loops is shipped as a default constant in the Rust service, overridable on the loop. Goal loops default to empty (the goal text is the prompt).
- Reuses the `Variant="secondary"` button style (matches existing dialogs) rather than introducing new variants.