# Sandboxed path resolution: per-call peer coverage (read-only)

## Goal

Extend the sandboxed path resolver so a tool call carrying `peer_session_id`
is allowed when the canonicalized requested path lies inside the
canonicalized directory of that named peer session. Coverage is **per-call**
(the `peer_session_id` argument on the tool call) and **read-only** (read,
glob, grep — and any future read-only tool that runs through the sandbox).

Mutating tools (write, edit, apply_patch, shell workdir) do **not** get
peer coverage from this change; they keep current behavior.

## Background

Today, `agents/src/tools/mod.rs::resolve_sandbox_path` only checks the
caller's session project root. Peer-targeted tool calls are routed
through `dispatch_peer_tool_if_requested`, which strips `peer_session_id`
and rebuilds a fresh `SandboxedContext` rooted at the peer's directory —
fine for tools that touch only the peer's tree, but the caller-side
resolver itself has no awareness of peer roots.

This plan adds per-call peer coverage to `resolve_sandbox_path` itself,
gated on:

- the tool call carrying a non-empty `peer_session_id`,
- the tool being read-only,
- the path canonicalizing inside the canonicalized directory of the
  named peer session.

## Design choices

- **Per-call, not per-membership.** Coverage is keyed off the
  `peer_session_id` argument on the current tool call, not off the
  routing virtual group. A caller can only read inside the tree of the
  peer they explicitly name. No "all connected peers are allowed" mode.
- **Read-only only.** Peer coverage does not apply to mutating tools.
  Those keep the existing `trigger_access` ask flow.
- **Canonicalized comparison.** Both the requested path (lexical if it
  doesn't exist on disk, real `std::fs::canonicalize` if it does) and the
  peer's directory are canonicalized before the `contains_path` check.
  This matches the existing local-root check.
- **No `SandboxPolicy` schema change.** Peer coverage is plumbed through
  `SandboxedContext` rather than added to `SandboxPolicy` / `external_roots`,
  so user-approved external directories and peer directories remain
  distinct categories in logs.

## Plan (in order)

1. **Plumb the tool call into `resolve_sandbox_path`.**
   - Change the signature to accept `Option<&ToolCall>` (or a small
     `PeerRef` struct that carries `peer_session_id` and a `tool` name).
   - Update every call site: `read_sandboxed`, `write_sandboxed`,
     `edit_sandboxed`, `apply_patch_sandboxed`, `glob_sandboxed`,
     `grep_sandboxed`. Mutating tools pass `None` so they never get
     peer coverage.

2. **Expose caller + peer directory lookup on `SandboxedContext`.**
   - Add `pub session_service: Arc<crate::SessionService>` and
     `pub caller_session_id: String` fields to `SandboxedContext`.
   - Add a builder method (`with_caller_id`) so existing
     `SandboxedContext::new` call sites compile unchanged; the
     `AgentService::build_runner_for_session` site in
     `src-tauri/src/services/agent_service.rs` populates the new fields
     from the session it just loaded.
   - Add a helper:
     ```rust
     impl SandboxedContext {
         fn peer_directory(&self, peer_session_id: &str) -> Option<PathBuf> { ... }
     }
     ```
     that calls `routing::integration::validate_connected_peer` (already
     used by `resolve_peer_tool_target`) and looks up the peer session's
     `directory` via `session_service.get_session(peer_id)`.

3. **Gate peer coverage on tool kind.**
   - In `resolve_sandbox_path`, before any peer check, ask the helper
     `is_read_only_tool(tool)` whether the tool is read-only
     (`read`, `read_file`, `glob`, `search_files`, `grep`).
   - Mutating tools never consult peer coverage, even if the call
     somehow carries `peer_session_id` (the async peer dispatcher already
     refuses non-read-only peers, so this is defense-in-depth).

4. **Add the peer check after the existing fast path.**
   - In `resolve_sandbox_path`, after the existing
     `sandbox.resolve(requested)` call returns Err *and* the error is
     an "outside root" error (`crate::sandbox::should_trigger_ask` is
     true), and the tool call carries a non-empty `peer_session_id`,
     and the tool is read-only:
     - Canonicalize the requested path (lexical if missing on disk,
       real canonicalize if it exists).
     - Resolve the peer's directory via the new helper and canonicalize
       it the same way.
     - If `sandbox::path::contains_path(&peer_root, &requested_canonical)`
       returns true, log `sandbox path resolution allowed via peer`
       with `caller_session_id`, `peer_session_id`, `peer_root`,
       `requested`, `canonical`, and return
       `Ok(requested_canonical)`.
   - Otherwise, fall through to the existing `trigger_access` ask flow
     unchanged.

5. **Refuse non-connected peer ids without asking.**
   - If the call carries a `peer_session_id` that isn't connected to the
     caller, fail the path resolution with a clear error
     (`peer_session_id 'X' is not connected to this session`) instead
     of triggering the ask. The existing routing helper
     `validate_connected_peer` already gives us the right error string.

6. **Logging + audit.**
   - Add a single info-level event for the peer-allowed path:
     `sandbox path resolution allowed via peer`.
   - Existing log events stay as-is. Make sure no existing field
     name collides with the new event so log filtering keeps working.

7. **Tests.**
   - New unit test: peer-connected, read-only tool, path inside the
     peer's canonical dir → allowed without an ask, logged via peer.
   - Regression: same read-only tool, path outside both caller root
     and peer root → triggers the ask (current behavior).
   - Regression: mutating tool (e.g. `write`) with a `peer_session_id`
     argument → does **not** consult peer coverage, still asks (or
     rejects) as today.
   - Regression: read-only tool with a `peer_session_id` that isn't
     connected → fails fast with the routing error, no ask.
   - Regression: structural error (e.g. symlink escape) inside the peer
     root still surfaces without an ask.

8. **Docs.**
   - Update the `SandboxedContext` doc comment to call out the
     per-call, read-only peer coverage rule.
   - One-line addition to `README.md` under Features:
     "Read-only tools may target a connected peer session by passing
     `peer_session_id`; the path is allowed without an external-root
     prompt as long as it canonicalizes inside that peer's directory."

## Files expected to change

- `agents/src/tools/mod.rs` — `resolve_sandbox_path`, every sandboxed
  tool wrapper, `SandboxedContext` struct + builder, new `peer_directory`
  helper, new tests.
- `src-tauri/src/services/agent_service.rs` — populate the new
  `SandboxedContext` fields at build time.

## Open questions

- Should we also let **plan-mode-only** mutating tools use peer coverage?
  No — keeping mutating tools strictly caller-rooted matches the README
  intent that peer targeting is for read-only context gathering.
- Cache peer directory lookups per call, or resolve fresh each time?
  Fresh each time. `validate_connected_peer` is cheap, and we want the
  newest directory in case the peer moved.

## Out of scope

- Grouping multiple peer ids into a single call. One call, one
  `peer_session_id`, one resolution.
- Mutating cross-peer writes/edits. Use the `task` tool to spawn a
  sub-session if you actually want a peer to mutate its own tree.
- Any change to `SandboxPolicy`, `external_roots`, or the
  user-facing "external directory" ask flow.