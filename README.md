# Meterix

A tray app that keeps track of how much credit you have left with your LLM API
providers.

It polls each provider on a timer, stores every reading, and puts a colour in
your system tray for the worst thing happening right now: teal when everything is
fine, amber when a balance has fallen under the threshold you set, copper when a
key has stopped working. There is a dashboard behind it for the detail, and a
popover on the tray icon when you only want a glance.

## What it does

- Checks every configured provider on a timer, 30 minutes by default, or on an
  interval you set for one provider alone. Intervals are honoured to within 30
  seconds.
- Watches the providers you tell it to. Switching one off stops the polling and
  takes it off the dashboard, and keeps both its key and every reading it has
  collected, so a provider you stop using for a while comes back whole.
- Shows each provider's remaining balance, where the number came from, and when it was read.
- Keeps a non-money figure apart from the money ones. ElevenLabs reports characters
  left rather than dollars, so it is shown with its own unit and left out of the
  total and the chart, the same way a spend figure is.
- Charts balance over time, using only readings from the credential you have now. Swap in a different account and the old readings are not drawn as one continuous line.
- Keeps a per-provider low threshold. Leave one blank and the provider's own
  published threshold applies — CheaperInference reports the balance it
  auto-recharges at — falling back to the app-wide default for a provider that
  publishes none.
- Sends a Windows notification when a balance crosses its threshold, and when a key is rejected. Each fires once per crossing, not once per poll.
- Can start at login.

Keys live in the OS keychain. They are never written to the database.

## Getting it

There are no published builds yet, so today you build it yourself. When there is
a release, the installer will need no extra runtime on Windows 10 or 11, since
WebView2 ships with Windows 11 and is a one-time download on 10.

### What you need

| | |
|---|---|
| Windows | 10 or 11. Linux and macOS are in the code but unverified. |
| Rust | 1.85 or newer. Cargo workspaces here use edition 2024. |
| Node | 22 or newer, for the frontend. |
| WebView2 | Installed already on Windows 11. The Evergreen runtime on 10. |

Nothing needs `libssl-dev` or OpenSSL. The dependency tree is rustls only, which
is deliberate: it keeps Linux packaging from needing system crypto headers.

### Build and run

```powershell
git clone <this repo>
cd app
npm install
npm run tauri dev
```

That opens the dashboard with the poller running. For a release build:

```powershell
cd app
npm run tauri build
```

The binary lands in `target/release/` at the repository root. The installer step
after it downloads NSIS on first use and writes to `target/release/bundle/`.

Everything is one Cargo workspace, so the whole project shares a single
`Cargo.lock` and a single `target/` directory. `cargo test --workspace` and
`cargo clippy --workspace` cover both the core and the app.

**Use `npm run tauri build`, not `cargo build --release`.** A bare cargo build
produces a dev-mode binary that loads the Vite dev server instead of the embedded
frontend, so it opens an Edge error page with no JavaScript running at all. It
looks like it worked. It cannot run.

## First run

The dashboard opens with an empty state and a key field. Paste a key, pick the
provider it belongs to, and press Save key. The app verifies the key against the
provider before storing it: a rejected key is not saved, and anything else is,
with a warning if no balance could be read.

Once a key is in, the provider gets a card, a line on the chart and a row in the
table.

## Where things live

| | |
|---|---|
| Database | `~/.meterix/meterix.db`, movable from the settings screen |
| Exports | CSV history, to a chosen folder; defaults to the data directory |
| Keys | Windows Credential Manager, service `meterix-core` |
| Settings | The `settings` table inside the same database file |

Set `METERIX_DB` to point at a different database. Useful for a second instance,
or for trying something without touching your real readings.

Removing a provider removes its keychain entry and its card, but keeps its stored
readings, so adding the key back later keeps everything together. A key can also
come from `OPENROUTER_KEY` or `CHEAPERINFERENCE_KEY`, which is how you track a
provider without saving anything to the keychain. A stored key wins if there is
one; the variable is the fallback.

## Settings

Reached from the dashboard header. There is no Save button on the page: changes
are held until you press Save Changes, which stays disabled until something
actually differs.

- **Check balances every.** 15 minutes to 6 hours.
- **Default low balance.** Used by any provider without its own threshold.
- **Two notification switches**, for low balances and for rejected keys.
- **Per-provider thresholds**, blank meaning "use the default".
- **Launch at login.**
- **Database path**, with a copy button.

## Command line

The core is a library with a thin CLI over it, useful for scripting and for
checking a key without opening the app.

```
meterix-cli fetch [provider]               read one provider, or all of them
meterix-cli history <provider> [limit]     recent readings, newest first
meterix-cli export [provider]              every reading as CSV, on standard output
meterix-cli providers                      what is tracked, and what is stored
meterix-cli add-gateway <name> --shape <shape> --base-url <url>
                                           track a gateway that speaks a known shape
meterix-cli remove-gateway <name>          remove one added that way
meterix-cli set-key <provider> <key>       store a key in the OS keychain
meterix-cli forget-key <provider>          remove a stored key
```

Providers are `openrouter`, `cheaperinference`, `deepseek` and `elevenlabs`. `limit`
defaults to 20, which
is about ten hours at the default poll rate.

## What it costs to run

Measured on a release build with two providers, ninety seconds after launch:

| | |
|---|---|
| The app's own process | 29 MB |
| WebView2, six processes | 374 MB |
| Total memory | about 403 MB |
| CPU while idle | 0.30 to 0.44% of one core |
| Launch plus a full refresh | 0.20 CPU-seconds |
| Binary | 15.8 MB |
| Database | 32 KB for 80 readings, about 10 KB a day at the default rate |

The CPU figure is the one that matters for something running all day: at a
30-minute poll that is roughly 7 CPU-seconds per day. Most of the memory is the
Edge runtime rather than anything this project builds.

## Limits

Worth knowing before you rely on it.

- **Four providers, and more by configuration.** Which providers are tracked is
  data: each one is a row you can switch off and back on, and a gateway that
  speaks a shape Meterix already understands can be added from the command line
  without rebuilding it. A provider whose API speaks a *new* shape is still a code
  change. That is the honest ceiling: no configuration describes OpenRouter's
  three-way choice between account credits, a key cap and spend.
- **Most providers do not publish a balance.** DeepSeek does, in money. ElevenLabs
  publishes a character allowance instead, which is shown in its own unit and left
  out of the total because it is not dollars. OpenAI and Anthropic publish spend
  rather than a balance, and only through an *admin* key, so they are not included;
  Groq publishes nothing at all, and AWS Bedrock has no balance endpoint and
  authenticates by request signature rather than a key.
- **No installer has been built.** The bundler fetches NSIS on first run, and that download has not completed in the environment this was developed in. The config is ready; nothing has been produced from it.
- **A removed key can come back.** Removing a provider deletes its keychain entry, but if the matching environment variable is set, the app keeps using it and the provider stays on the list. The app says which variable, since it cannot unset one for you.
- **Dark only.** There is no light theme.
- **Native title bar.** The reference design has a custom one; the window uses the Windows chrome.
- **Thresholds do not re-open notifications.** A key that gets rejected notifies; a balance crossing notifies. Editing a threshold recolours the tray straight away, but the toast for it arrives at the next poll, because a toast is tied to a reading.
- **The chart has ten colours.** Past ten providers, colours repeat.
- **Cross-platform is untested.** Linux needs an AppIndicator host for the tray and a Secret Service for keys.

## Development

```powershell
cargo test --workspace          # 52 tests: 48 in the core, 4 in the app crate
cargo clippy --workspace --all-targets
cd app && npx tsc --noEmit
```

The core owns everything that talks to a provider, stores a reading or decides
what a balance means. The Tauri crate owns windows, the tray and notifications.
The React app owns presentation and nothing else, which is why it holds no
thresholds and no copy for what a provider error means.

`docs/PROJECT.md` is the longer document: data model, the reasoning behind
decisions that would otherwise look arbitrary, and an honest list of gaps.
`mockup/index.html` is the design reference, showing all three surfaces at once.

## Layout

```
crates/meterix-core/  the core: a library, plus a thin CLI called meterix-cli
app/                  the desktop app
  src/                React dashboard, popover and settings
  src-tauri/          Tauri shell: windows, tray, notifications, poller
website/              the landing page: one page, no build step, no dependencies
mockup/index.html     the whole app on one page, as a design reference
docs/PROJECT.md       design decisions, data model, known gaps
Cargo.toml            one workspace: shared versions, one lockfile, one target/
```
