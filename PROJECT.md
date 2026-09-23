# Meterix

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
| UI | React, TypeScript, Tailwind 4 |
| Storage | SQLite via rusqlite, one local file |
| Secrets | OS keychain via `keyring` |
| HTTP | reqwest |
| Scheduler | `tokio::time::interval` |
| Packaging | Tauri bundler: `.dmg`, `.msi`, `.deb`, `.AppImage` |

Type comes from Inter and JetBrains Mono, self-hosted through Fontsource rather
than fetched from a CDN, because a desktop app should not depend on the network
to look right.

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
| 3 | React dashboard over Tauri commands | done |
| 4 | Background poller, tray icon, popover | done |
| 5 | Trend chart, per-provider threshold, OS notification | chart and thresholds done, notifications pending |
| 6 | Settings screen, more providers, packaging | settings done, more providers and packaging not started |

## Layout

```
src/               the core: library plus a CLI front end
app/               the desktop app
  src/             React dashboard
  src-tauri/       Tauri shell, the only place that knows about both sides
mockup/index.html  the static design reference the app was built from
```

The core is a library with a thin CLI, and the Tauri shell is a third entry
point over the same library. `app/src-tauri` declares its own empty workspace
so it cannot disturb the core crate.

## Running it

```
cd app
npm install
npm run tauri dev      # dev window against the live API
npm run tauri build    # installers
```

The dashboard starts by reading whatever is already in the database, then
refreshes from the providers if any key is configured. Keys are added in the
app, and stored in the OS keychain rather than the database.

## Commands

```
meterix-core fetch [provider]              fetch one provider, or all of them
meterix-core history <provider> [limit]    recent snapshots, newest first
meterix-core set-key <provider> <key>      store an API key in the OS keychain
meterix-core forget-key <provider>         remove a stored API key
```

`limit` defaults to 20. At a 30-minute poll that is roughly ten hours, so pass a
bigger number when looking at a week or a month.

Providers are `openrouter` and `cheaperinference`.

## Data model

As built, which is not what the original draft said:

```sql
providers (
  id, name, low_balance_threshold
)

balance_snapshots (
  id, provider_id, remaining, basis, account_credits, usage, recorded_at
)

settings (
  key, value
)
```

`providers.low_balance_threshold` is nullable, and null is the normal case: it
means "use the app default". Zero would mean "never warn me", which is a
different thing, so the column stays null rather than defaulting to a number.

`settings` is a free-form key/value table, currently holding
`poll_interval_minutes` and `low_balance_threshold`. Unknown keys are ignored on
read rather than being an error, so an older build opening a newer database
loses nothing.

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

**Polling runs whether or not a window is open.** A background task refreshes
every 30 minutes and the app does not exit when its windows close — closing the
dashboard hides it, and only Quit in the tray menu actually stops the process.
That is what makes it a watcher rather than a viewer, and it is why the chart
and the burn rate have anything to plot at all.

The poller, the tray menu and the Refresh button all go through one
`refresh_and_store`, so they cannot disagree. Every refresh emits
`balances-updated`, and windows re-read the database rather than being handed
the payload, so there is one path for reading state.

**The tray icon is drawn in code.** Four colours, one 32×32 three-bar mark
rendered into an RGBA buffer per status, so there are no icon files to keep in
step. It shows the worst status across the providers, because one failing
provider deserves more attention than another one being healthy.

**A key is checked before it replaces anything.** Saving verifies the candidate
against the provider first, and the existing keychain entry is left untouched
until it passes. A provider that refuses the credential (401) writes nothing at
all, so a typo cannot destroy a working key. A provider that merely cannot be
reached does **not** block the save, because the key is not known to be wrong and
refusing would leave someone offline unable to set one.

Three outcomes, and the difference matters: `saved_verified` (works, balance
read), `saved_unverified` (stored, no balance readable), `rejected` (nothing
written). The decision lives in `save_verified_key` in the core so the CLI and
the app cannot disagree about it.

**A key filed under the wrong provider is called out.** Refusing a key that
belongs to another provider produces a bare "rejected", which sends someone to
check a key that was fine. `key_prefix` in the `ProviderSpec` table drives a
hint: "It starts with `ci_`, the CheaperInference format, so check which provider
is selected." Prefixes are conventions, not contracts, so this only ever
produces a hint and never decides anything.

**The keychain needs an explicit backend feature, and this was a real bug.**
`keyring` has no default features, and when no platform backend is selected it
silently falls back to an in-process mock store. That store accepts a write,
returns success, and forgets the value when the process exits. A saved API key
looked fine and was gone by the next launch, with no error anywhere. `Cargo.toml`
now selects `windows-native`, `apple-native` or `sync-secret-service` per
platform. On Linux `crypto-rust` is chosen over `crypto-openssl` so no OpenSSL
dependency appears in the tree. `save_key` also reads the value back after
writing, because a keychain that accepts a write and cannot return it is worse
than one that refuses outright.

**One name.** The product is Meterix. The core crate is `meterix-core`, the app
crate is `meterix`, the keychain service and data directory are both
`meterix-core`. Nothing was renamed after keys were stored, so no key was
stranded.

**The dashboard only plots readings that are balances.** Rows whose `basis` is
`usage` hold spend, which climbs as the account empties. Drawing one on the
same axis as a balance produces a chart that looks healthy while the money runs
out. Those rows are filtered out of the chart and counted underneath it, and
they show as grey ticks on a provider card rather than coloured ones.

**Removing a provider deletes the key and keeps the readings.** Tidying up a
key list should not silently destroy a history. Nothing reads the history of a
provider that is gone, so it costs a few rows to leave it alone, and re-adding
the key brings its chart back.

**A failed fetch in one provider does not discard the others.** Each provider's
error is reported on its own card and the rest still refresh.

**Low is a per-provider threshold, resolved in one place.** `effective_threshold`
is the only thing that decides whether a balance counts as low, and it resolves a
provider's own value against the app default. The resolved number travels out on
the provider payload, so the tray and the dashboard read the same field instead of
each keeping a constant. That duplication is gone: there is exactly one literal
left (`DEFAULT_LOW_BALANCE_THRESHOLD` in the core) and no threshold in TypeScript
at all.

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
credential, a rejected key, a key without permission, rate limiting, an
unreachable host and an unusable response. The v1 dashboard needs to tell those
apart, and the v2 tray icon is colour-coded off them. `anyhow` is only used at
the top level, where nothing needs to branch.

A provider's own explanation is kept rather than replaced. "API key scope
required: account:read" tells someone what to go and fix; "HTTP 403" does not.
Provider error bodies are parsed for a message, and HTML error pages are
discarded rather than repeated back.

## Provider notes

Checked against the live APIs rather than the documentation, because the two
disagreed.

**OpenRouter.** `GET /api/v1/key` returns `limit`, `limit_remaining` and
`limit_reset` as nullable per-key caps. `null` means no cap was configured,
which is normal, and the first version wrongly treated it as a parse failure.
`GET /api/v1/credits` gives the real account balance and is called on every
fetch despite the docs saying it needs a management key.

**CheaperInference.** The base URL is `https://api.cheaperinference.com/v1` and
the balance endpoint is `GET /v1/account/balance` with `Authorization: Bearer`.
Both were confirmed: a wrong path returns the marketing site's HTML 404, while
the real path returns a structured JSON error.

The catch is scope. An **inference-only key authenticates but cannot read a
balance** — it answers `403 insufficient_scope` asking for `account:read`.
`GET /v1/models` succeeds with any valid key, so that is the quick way to tell
"the key is fine, the scope is missing" from "the key is wrong". A key needs the
`account:read` scope for this provider to be trackable at all.

Spend comes from `GET /v1/account/usage?days=N`, which reports `billed_usd`,
`list_usd` and `saved_usd` over a **window**. The window is capped at 90 days —
`days=365` is a 400 — so it is never a lifetime figure the way OpenRouter's
`usage` is. Every snapshot therefore records `spend_window_days` next to the
spend, and the dashboard only sums all-time figures into its headline. Adding a
90-day figure to an all-time one would produce a total that means nothing.

The balance endpoint also reports `reserved_usd`, `threshold_usd` and
`recharge_amount_usd`. The app reads `available_usd`, which is what the account
can actually spend after reservations.

## Open questions

- Poll interval, default 30 minutes. Some providers rate-limit balance checks,
  so it may need to be per provider rather than global.
- What counts as "low"? A raw USD threshold is the simplest thing that works.
  A percentage of the last top-up would need top-up history, which is not stored.
- Linux tray support varies by desktop environment. Deal with it at step 4.

## Known gaps

- Launch at login is built and verified, but enabling it from a `tauri dev` run
  registers the **debug binary** as the login item, so the entry stops working
  once that build is cleaned. It writes the right path when the app is installed;
  treat it as untrustworthy until packaging exists.
- The poll interval is configurable from the settings screen, in the range
  1-1440 minutes. Changing it wakes the poller's sleep rather than waiting out
  the old interval, so the next check happens on the new schedule.
- The low threshold is per provider now, resolved by `effective_threshold`. The
  header shows a single number only when every provider agrees on one, and says
  "thresholds per provider" otherwise, because printing one of two different
  numbers would be a quiet lie.
- The chart fills each series from its line down to the bottom of the plot, and
  draws the gridlines underneath the fills. Once more than a handful of readings
  exist the fills cover the whole plot and the gridlines with it, so the chart
  reads as a wall of colour rather than a trend. Filling from zero is the wrong
  choice for balances that never approach zero. Not fixed.
- Nothing records *which* key produced a snapshot, only which provider. Swapping
  an account mid-history splices two accounts into one trend line.
- "Remove a provider" removes the keychain entry but not an environment
  variable, so a provider can stay configured after being removed.
- The popover reads the database when it mounts and on every refresh event, not
  when it is opened, so it can be up to one poll interval old if nothing else has
  refreshed. It has a Refresh button for that.
- No tray icon on Linux without an AppIndicator host, which is a desktop
  environment question rather than a code one.
- Light mode is not built. The palette in `app/src/index.css` is dark only.
- The window keeps its native title bar rather than the reference design's
  custom one, so a Windows title bar sits above the app header.
- No settings screen, despite a button for one in the mockup.
- "Remove a provider" removes a key. Since the set of supported providers is
  compiled in, that is all it can mean until providers are data.
- A headless or minimal Linux install may have no Secret Service, so `keyring`
  will fail. Cross-platform is not free on that leg.
- Schema migrations are `PRAGMA table_info` plus `ALTER TABLE`. Fine for a
  handful. Move to a `PRAGMA user_version` ladder once there are three or more.
- The bundled CSP is off (`csp: null`). Worth tightening before packaging.
