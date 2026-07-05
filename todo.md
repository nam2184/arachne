# Todo: Codex provider — switch account / OAuth profile manager

**Branch:** `feature/codex-switch-account`
**Owner:** Aiden Cao (`nam2184`)
**Status:** planning — no code yet

## Problem

Today the OpenAI provider (which is what "Codex" auth uses — note the `codex_cli_simplified_flow=true` and `id_token_add_organizations=true` extra params in `ProviderOAuthConfig`) supports exactly **one OAuth session per provider name** in `ProviderAuthState`:

```rust
// agents/src/database/repositories.rs (ProviderAuthState)
pub access_token: Option<String>,
pub refresh_token: Option<String>,
pub account_id: Option<String>,
```

If you have two ChatGPT/Codex accounts (e.g. personal + work, or two orgs), you have to fully revoke + re-auth every time you want to swap. There's no UI affordance for multiple profiles, and `ProviderAuthStateRepository::find_by_provider` returns a single row keyed by `provider_name`.

## Goal

Let users save **named OAuth profiles** per Codex provider, switch between them without re-authenticating, and persist the choice across restarts.

## Scope

**In scope:**
- Multiple OAuth profiles for the Codex/OpenAI provider, each with its own tokens + account_id + user-chosen label
- One "active" profile at a time, used by the runtime provider
- UI to: name a profile during login, list profiles, switch, rename, delete
- Persistence (SQLite, via existing migrations)
- Migration of any existing single OAuth session → default profile named "Default"

**Out of scope (for this PR):**
- Multi-provider support beyond OpenAI/Codex (Anthropic OAuth, etc.)
- Profile-level base_url / model overrides (those stay on `ProviderConfig`)
- Cross-device profile sync
- Profile import/export

## Design (proposed)

### Data model

New table `provider_oauth_profiles`:

| column | type | notes |
|---|---|---|
| `id` | TEXT PK | uuid |
| `provider_name` | TEXT NOT NULL | e.g. `openai` |
| `label` | TEXT NOT NULL | user-chosen, e.g. "Work", "Personal" |
| `access_token` | TEXT | nullable until first auth |
| `refresh_token` | TEXT NULL | |
| `account_id` | TEXT NULL | from JWT claim, used for display |
| `created_at` | INTEGER | unix ms |
| `last_used_at` | INTEGER NULL | updated on switch/use |
| `is_active` | INTEGER NOT NULL | 0/1 — at most one active per provider |

Indexes:
- `(provider_name, is_active)` — fast "give me the current one"
- `(provider_name, label)` UNIQUE — no duplicate labels per provider

Replace `ProviderAuthState`'s token fields for OAuth providers with a foreign reference: `active_oauth_profile_id`. Keep `api_key` path unchanged.

### Rust layer

- New `agents/src/provider_oauth_profiles.rs`:
  - `ProviderOAuthProfile` struct (serde, sqlx)
  - `ProviderOAuthProfileRepository` (CRUD + `set_active(provider_name, profile_id)`)
- Extend `ProviderOAuthCoordinator` with optional `profile_label: Option<String>` arg to `start()`; on `complete()`, create the profile row with that label.
- `ProviderService::complete_oauth` returns the new profile (with id + label), not a bare `ProviderAuthState`.
- New `ProviderService` methods:
  - `list_oauth_profiles(provider_name) -> Vec<ProviderOAuthProfile>`
  - `set_active_oauth_profile(provider_name, profile_id)`
  - `rename_oauth_profile(profile_id, new_label)`
  - `delete_oauth_profile(profile_id)` — refuses to delete the active one
- Where `config.api_key = auth.selected_token()` runs, switch to `config.api_key = active_profile.access_token` when auth type is OAuth and a profile exists.

### Frontend (`desktop/`)

- `SettingsPage.tsx` — in the Codex/OpenAI provider card, replace the single "Sign in" button with:
  - Current profile chip: `<label> · <account_id short>`
  - Dropdown menu: Switch account → list of profiles → "Add another account…" → opens existing OAuth flow with a label input prompt
  - Hover row actions: rename, delete
- New Tauri commands mirror the Rust service methods above.
- Optimistic UI: switching updates the chip immediately, rolls back on error.

### Migration

- `agents/migrations/<ts>_oauth_profiles.sql` creates the table.
- One-time data migration: if a `provider_auth_states` row exists for `openai` with non-empty OAuth tokens, copy into a new profile named "Default" and set it active. Then keep `provider_auth_states` for the api_key path only.

## Tasks

### Backend (Rust)

- [ ] Add `ProviderOAuthProfile` struct + `ProviderOAuthProfileRepository`
- [ ] Write SQL migration + wire into `Database::init`
- [ ] Backfill migration (legacy `ProviderAuthState` OAuth row → default profile)
- [ ] Extend `ProviderOAuthCoordinator::start/complete` to accept + persist a profile label
- [ ] Update `ProviderService::complete_oauth` to return the new profile
- [ ] Add `list/set_active/rename/delete` methods on `ProviderService`
- [ ] Wire `ProviderConfig.api_key` to derive from the active profile when auth_type = OAuth
- [ ] Unit tests:
  - [ ] Backfill creates one "Default" profile when legacy data exists
  - [ ] `set_active` swaps the active flag transactionally
  - [ ] `delete` refuses when the profile is active
  - [ ] Duplicate-label insert rejected

### Frontend (React + Tauri)

- [ ] Add Tauri commands for the new service methods (with proper error types)
- [ ] Replace Codex OAuth card UI with profile list + add-account flow
- [ ] Add a "label this account" prompt on the OAuth callback page (after `/auth/callback` succeeds, before window closes)
- [ ] Show account_id short form in the chip (e.g. `acc_abc…xyz`)
- [ ] Settings page reload on profile change so other panels (model picker, etc.) update
- [ ] Visual confirmation toast on switch

### QA

- [ ] Manual: sign in with account A → "Work" → switch → sign in with account B → "Personal" → switch back to "Work" without re-auth
- [ ] Persistence: restart app, profile labels + active selection survive
- [ ] Migration: fresh DB upgrade from pre-feature build keeps the existing single session working
- [ ] Edge case: delete the active profile via API → returns error, UI greys out the button
- [ ] Edge case: refresh token expires → existing refresh-on-401 logic continues to work per-profile
- [ ] Security review: tokens still never logged in plaintext, label-input is length-bounded + sanitized

### Docs

- [ ] Update `README.md` "Codex / OpenAI" section with switch-account flow
- [ ] Note in CHANGELOG (if it exists)

## Open questions

1. **Profile-level model override?** Right now model lives on `ProviderConfig`. Should each profile remember the model last used with it? — *defer, out of scope v1.*
2. **Default profile auto-name?** "Default" vs asking user every time on first login? — *proposal: ask once on first ever login, "Default" only on auto-backfill.*
3. **Delete vs deactivate?** Should `delete` soft-delete (mark inactive, keep tokens for re-activation)? — *proposal: hard delete in v1, can add soft delete later if requested.*
4. **Profile ordering in UI?** Recently-used first, alphabetical, manual sort? — *proposal: active pinned to top, then by last_used_at desc.*

## Risks

- **Token leakage in logs** — must audit all `tracing::debug!` sites that touch tokens; ensure profile lookup doesn't accidentally log the full access_token
- **OAuth coordinator state** — currently keys pending flows by `provider_name` only. If two OAuth flows start for the same provider in parallel (e.g. user clicks "Add another" twice), the second aborts the first. That's the right behavior for now, but worth noting.
- **Migration ordering** — feature must be additive; users with existing single-session must not lose access during upgrade.

## Estimated effort

- Backend: ~1 day (struct, repo, migration, service wiring, tests)
- Frontend: ~1 day (commands, UI, toast/refresh wiring)
- QA + docs: ~0.5 day
- **Total: ~2.5 dev days**, plus review buffer

## Files expected to change

```
agents/migrations/<ts>_oauth_profiles.sql            (new)
agents/src/database/repositories.rs                  (new repo)
agents/src/database/mod.rs                           (re-export)
agents/src/provider_oauth.rs                         (accept label)
agents/src/provider_oauth_profiles.rs                (new)
agents/src/provider_service.rs                       (new methods, OAuth lookup)
agents/src/lib.rs                                    (re-export)
agents/src/domain.rs                                 (struct, if shared)
desktop/app/layout/components/SettingsPage.tsx       (UI rewrite)
desktop/app/layout/components/CodexAccountSwitcher.tsx  (new)
desktop/lib/tauri/commands.ts                        (new commands)
README.md                                            (docs)
todo.md                                              (this file)
```