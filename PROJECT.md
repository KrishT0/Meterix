# Plexo Credits

A tray app that watches how much credit is left on your LLM API accounts. It
polls each provider in the background, colour-codes the tray icon, and shows a
dashboard when you click it.

## Why this exists

CostGoat does most of this, but it's closed source, paid, and Mac-leaning.
CodexBar is open source but only for macOS. Nothing free and open covers
Windows, Mac and Linux at once. So: build it, own it, learn Rust doing it.

## What it is not

- Not a cost calculator or a token-usage analyser. Providers already report that.
- Not a proxy. It reads balance and never routes a request.
- No accounts, no cloud sync, no multi-device. One local SQLite file per machine.
- No mobile app.

## Stack

| Piece | Choice |
|---|---|
| Core | Rust |
| Shell | Tauri v2 |
| UI | React, TypeScript, Tailwind |
| Storage | SQLite via rusqlite, one local file |
| Secrets | OS keychain via `keyring` |
| HTTP | reqwest |
| Scheduler | `tokio::time::interval` |
| Packaging | Tauri bundler: `.dmg`, `.msi`, `.deb`, `.AppImage` |

Rust is a deliberate choice rather than a means to an end. Ownership, async,
`Result` and traits are the things this project is meant to teach.

The dependency tree resolves to rustls. There is no `openssl-sys` and no
`native-tls`, which means Linux packaging does not need `libssl-dev`. Watch
that if a dependency gets added later.

## Build order

Each step is meant to run before the next one starts.

| Step | Scope | Status |
|---|---|---|
| 1 | Provider trait, OpenRouter and CheaperInference adapters, CLI | done |
| 2 | SQLite snapshots, keychain key storage | done |
| 3 | React dashboard over Tauri commands | not started |
| 4 | Background poller, tray icon, popover, autostart | not started |
| 5 | Trend chart, per-provider threshold, OS notification | schema ready |
| 6 | Settings, more providers, packaging | not started |

## Commands

```
meterix-core fetch [provider]              fetch one provider, or all of them
meterix-core history <provider> [limit]    recent snapshots, newest first
meterix-core set-key <provider> <key>      store an API key in the OS keychain
```

`limit` defaults to 20. At a 30-minute poll that is roughly ten hours, so pass a
bigger number when looking at a week or a month.

Providers are `openrouter` and `cheaperinference`.

## Data model

As built, which is not what the original draft said:

```sql
providers (
  id, name
)

balance_snapshots (
  id, provider_id, remaining, basis, account_credits, usage, recorded_at
)
```

The draft imagined a single `available_usd` column. That turned out to be
impossible to fill honestly: OpenRouter has no single number that means
"balance", and what it does return depends on how your key is set up.

`basis` says what `remaining` actually is, and it matters. Read it before
treating `remaining` as money.

| `basis` | `remaining` holds | Is it a balance? |
|---|---|---|
| `account_credits` | credits bought minus credits used | yes |
| `key_cap` | allowance left under a spending cap on the key | yes |
| `usage` | spend so far, which only ever grows | no |

Anything charting or displaying `remaining` should filter on `basis` first.
Plotting a `usage` row as a balance gives you a line that climbs while your
money runs out.

`account_credits` and `usage` keep the raw provider figures alongside, and are
null when the provider did not report them. `recorded_at` is UTC.

API keys are never in this database. They live in the OS keychain, under the
service name `meterix-core`, keyed by provider name.

The database lives in the per-user app data directory, not the working
directory, because a packaged app is launched with an arbitrary CWD and may be
installed somewhere read-only.

- Windows: `%APPDATA%\meterix-core\meterix.db`
- macOS: `~/Library/Application Support/meterix-core/meterix.db`
- Linux: `$XDG_DATA_HOME/meterix-core/meterix.db`, or `~/.local/share/...`

`METERIX_DB` overrides the whole path.

## Decisions worth knowing

**OpenRouter balance precedence is account credits, then key cap, then usage.**
Credits are the real balance. A key cap is not the account balance, but it is
still money the key can spend, so it outranks spend-so-far. Usage is last and
gets labelled as not being a balance.

This was found the hard way. `GET /api/v1/key` returns `limit`, `limit_remaining`
and `limit_reset` as nullable per-key caps. `null` means no cap was configured,
which is normal for a personal key. The first version treated that null as a
parse error and aborted, so no fetch ever succeeded.

**`/credits` is called on every fetch, despite the docs.** OpenRouter's
documentation says the credits endpoint needs a management key. A personal key
was observed returning it successfully, so gating the call behind
`is_management_key` was tried and then removed. One wasted request costs less
than silently losing the account balance.

**A failure in one provider does not discard the others.** Credentials are
resolved per provider, each provider's error goes to stderr, and the run only
fails when nothing succeeded. The earlier version aborted the whole run on the
first error, which meant a missing key for one provider also threw away the
other provider's balance.

**Errors are typed, not strings.** `ProviderError` distinguishes a missing
credential, a rejected key, rate limiting, an unreachable host and an unusable
response. The v1 dashboard needs to tell those apart, and the v2 tray icon is
colour-coded off them. `anyhow` is only used at the top level, where nothing
needs to branch.

## Open questions

- **Naming.** The directory, the crate, the binary and the keychain service all
  say `meterix`. The product is Plexo Credits. Pick one before packaging. The
  keychain service name is user-visible state, so changing it later strands
  every stored key.
- Poll interval, default 30 minutes. Some providers rate-limit balance checks,
  so it may need to be per provider rather than global.
- What counts as "low"? A raw USD threshold is the simplest thing that works.
  A percentage of the last top-up would need top-up history, which is not stored.
- Linux tray support varies by desktop environment. Deal with it at step 4.

## Known gaps

- No README-level onboarding: what a new user does first is undefined.
- Adding a provider is a CLI flag. There is no interactive flow.
- Removing a provider does not exist, neither the keychain entry nor the rows.
- Nothing surfaces "last checked", though the data is there.
- A headless or minimal Linux install may have no Secret Service, so `keyring`
  will fail. Cross-platform is not free on that leg.
- Schema migrations are `PRAGMA table_info` plus `ALTER TABLE`. Fine for a
  handful. Move to a `PRAGMA user_version` ladder once there are three or more.
