//! Tauri shell over `meterix-core`.
//!
//! Nothing clever lives here. The commands translate between the core's types
//! and JSON, and the frontend does the presenting. Anything this file starts
//! doing on its own is a sign it belongs in the library instead.

// Release builds should not open a console window behind the app. Debug keeps
// it, because that is where Rust errors and panics show up.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tauri_plugin_notification::NotificationExt;
// Brings `app.dialog()` into scope for the folder picker.
use tauri_plugin_dialog::DialogExt;

use meterix_core::{
    Basis, MISSING_CREDENTIAL_KIND, Notice, PROVIDERS, Settings, Snapshot, credential_hint,
    adopt_legacy_database, display_name, due_providers, export_directory, fallback_thresholds,
    fetch_selected, first_reading_at, forget_key, history, history_csv, load_settings, open_database,
    provider_fingerprint, provider_intervals,
    provider_thresholds, record_attempts, requested_providers, resolved_thresholds,
    save_settings as persist_settings, save_snapshot, save_verified_key,
    set_provider_interval as store_interval, set_provider_threshold as store_threshold,
    take_notifications,
};

/// One provider, as the dashboard needs it: whether a key exists, and the most
/// recent reading if there is one.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderOverview {
    /// The id, as used in the database and on the command line.
    name: String,
    /// How the provider is written for a person.
    display_name: String,
    configured: bool,
    /// The stored key's format prefix and last four characters, if there is one.
    key_hint: Option<String>,
    balance: Option<f64>,
    basis: Option<Basis>,
    account_credits: Option<f64>,
    usage: Option<f64>,
    /// Days the usage figure covers. Null means all-time.
    spend_window_days: Option<u32>,
    recorded_at: Option<String>,
    /// Balance below which this provider counts as low, already resolved from
    /// its own override or the app default. Sent so the dashboard never has to
    /// hold a threshold of its own.
    threshold: f64,
    /// Labels the credential in use, so the chart can plot one account's history
    /// rather than splicing two together. Never the key itself.
    key_fingerprint: Option<String>,
}

/// What one provider did on a refresh. Failures travel alongside successes
/// rather than aborting the whole batch.
///
/// `Clone` so the last batch can be kept for the tray: changing a threshold has
/// to be able to recolour the icon without going back to the network.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RefreshOutcome {
    provider: String,
    display_name: String,
    ok: bool,
    balance: Option<f64>,
    basis: Option<Basis>,
    account_credits: Option<f64>,
    usage: Option<f64>,
    spend_window_days: Option<u32>,
    error_kind: Option<String>,
    error_message: Option<String>,
}

/// The outcomes of the last refresh.
///
/// Kept so that changing a threshold can recolour the tray straight away. A
/// threshold moves no balance — only the comparison against one — so going back
/// to the network would be slower and would record a reading that is not a poll.
#[derive(Default)]
struct LastOutcomes(Mutex<Vec<RefreshOutcome>>);

impl LastOutcomes {
    fn store(&self, rows: &[RefreshOutcome]) {
        if let Ok(mut last) = self.0.lock() {
            *last = rows.to_vec();
        }
    }
}

#[tauri::command]
fn overview() -> Result<Vec<ProviderOverview>, String> {
    let connection = open_database().map_err(|error| error.to_string())?;
    let settings = load_settings(&connection).map_err(|error| error.to_string())?;
    // Resolved in one place, so a provider cannot be low on the dashboard and
    // fine in the tray.
    let thresholds = resolved_thresholds(&connection).map_err(|error| error.to_string())?;

    PROVIDERS
        .iter()
        .map(|name| {
            let latest = history(&connection, name, 1)
                .map_err(|error| error.to_string())?
                .into_iter()
                .next();

            // One keychain read rather than two: the hint answers both "what is
            // stored" and "is anything stored".
            let hint = credential_hint(name);

            let threshold = thresholds
                .iter()
                .find(|(provider, _)| provider == name)
                .map_or_else(
                    || settings.low_balance_threshold,
                    |(_, threshold)| *threshold,
                );

            Ok(ProviderOverview {
                name: (*name).to_string(),
                display_name: display_name(name).to_string(),
                configured: hint.is_some(),
                key_hint: hint,
                balance: latest.as_ref().map(|snapshot| snapshot.remaining),
                basis: latest.as_ref().map(|snapshot| snapshot.basis),
                account_credits: latest.as_ref().and_then(|snapshot| snapshot.account_credits),
                usage: latest.as_ref().and_then(|snapshot| snapshot.usage),
                spend_window_days: latest.as_ref().and_then(|snapshot| snapshot.spend_window_days),
                recorded_at: latest.map(|snapshot| snapshot.recorded_at),
                threshold,
                key_fingerprint: provider_fingerprint(name),
            })
        })
        .collect()
}

#[tauri::command]
/// Fetch, store, and report. Shared by the Refresh button, the tray menu and
/// the poller, so none of them can drift apart.
async fn refresh_and_store(
    app: &AppHandle,
    only: Option<&str>,
) -> Result<Vec<RefreshOutcome>, String> {
    let selection = requested_providers(only).map_err(|error| error.to_string())?;

    refresh_selected(app, &selection).await
}

/// Fetch, store and report an explicit set of providers.
///
/// The poller is why this takes a set rather than one name: it fetches everything
/// that is due in a single pass, because notifications are worked out across the
/// whole batch — two providers crossing a threshold in the same check are one
/// message rather than two.
async fn refresh_selected(
    app: &AppHandle,
    providers: &[&'static str],
) -> Result<Vec<RefreshOutcome>, String> {
    // Fetch first and open the database afterwards. A rusqlite `Connection` is
    // Send but not Sync, so holding one across an await would make this future
    // non-Send and stop the command from compiling.
    let outcomes = fetch_selected(providers)
        .await
        .map_err(|error| error.to_string())?;

    let connection = open_database().map_err(|error| error.to_string())?;
    // Recorded before the readings are stored, and on every path that fetches
    // rather than only in the poller: a manual refresh should push the next
    // automatic one out too, and a provider that fails has to back off for its
    // own interval instead of being retried on the next beat.
    if let Err(error) = record_attempts(&connection, providers) {
        // Not fatal. Losing the timestamp costs one extra fetch; refusing to go
        // on would cost the readings.
        eprintln!("could not record the attempt: {error}");
    }

    let thresholds = resolved_threshold_map(&connection)?;
    let mut rows = Vec::with_capacity(outcomes.len());

    for (provider, result) in &outcomes {
        match result {
            Ok(balance) => {
                save_snapshot(&connection, balance).map_err(|error| error.to_string())?;

                rows.push(RefreshOutcome {
                    provider: provider.to_string(),
                    display_name: display_name(provider).to_string(),
                    ok: true,
                    balance: Some(balance.remaining),
                    basis: Some(balance.basis),
                    account_credits: balance.account_credits,
                    usage: balance.usage,
                    spend_window_days: balance.spend_window_days,
                    error_kind: None,
                    error_message: None,
                });
            }
            Err(error) => rows.push(RefreshOutcome {
                provider: provider.to_string(),
                display_name: display_name(provider).to_string(),
                ok: false,
                balance: None,
                basis: None,
                account_credits: None,
                usage: None,
                spend_window_days: None,
                error_kind: Some(error.kind().to_string()),
                error_message: Some(error.to_string()),
            }),
        }
    }

    update_tray(app, &rows, &thresholds);

    // Remembered so a later threshold change can recolour the tray from these
    // readings instead of fetching them again.
    if let Some(cached) = app.try_state::<LastOutcomes>() {
        cached.store(&rows);
    }

    // Deliberately after the tray and the snapshots, so a notification that
    // cannot be shown never costs a recorded reading. The edge is consumed even
    // if showing it fails, which is why this does not depend on the result.
    match load_settings(&connection) {
        Ok(settings) => match take_notifications(&connection, &outcomes, &settings) {
            Ok(notices) => notify(app, &notices),
            Err(error) => eprintln!("could not work out notifications: {error}"),
        },
        Err(error) => eprintln!("could not read settings: {error}"),
    }

    // Tell every open window, so the popover and the dashboard cannot disagree.
    // They re-read the database rather than being handed this payload, which
    // keeps one path for reading state.
    let _ = app.emit(UPDATED_EVENT, ());

    Ok(rows)
}

/// Turn notices into the toasts the user actually sees.
///
/// Two providers crossing in the same check become one toast instead of two. Two
/// toasts stacked in the corner read as noise, and the second is usually pushed
/// off screen before it can be read anyway. Foreground and background both read
/// the same wording, because a notification that arrives while the dashboard
/// happens to be open is not a different event.
fn notify(app: &AppHandle, notices: &[Notice]) {
    let show = |title: String, body: String| {
        if let Err(error) = app.notification().builder().title(title).body(body).show() {
            eprintln!("could not show a notification: {error}");
        }
    };

    let low: Vec<(&str, f64, f64)> = notices
        .iter()
        .filter_map(|notice| match notice {
            Notice::LowBalance {
                display_name,
                remaining,
                threshold,
                ..
            } => Some((display_name.as_str(), *remaining, *threshold)),
            Notice::KeyError { .. } => None,
        })
        .collect();

    match low.as_slice() {
        [] => {}
        [(name, remaining, threshold)] => show(
            format!("{name} is running low"),
            format!("${remaining:.2} left, below your ${threshold:.2} threshold."),
        ),
        several => {
            // "Both" is only true while there are two providers to have.
            let title = if several.len() == 2 && PROVIDERS.len() == 2 {
                "Both providers are running low".to_string()
            } else {
                format!("{} providers are running low", several.len())
            };

            let body = several
                .iter()
                .map(|(name, remaining, _)| format!("{name} ${remaining:.2}"))
                .collect::<Vec<_>>()
                .join(" · ");

            show(title, body);
        }
    }

    // One each, rather than combined: these are separate keys to go and replace.
    for notice in notices {
        if let Notice::KeyError {
            display_name,
            error_kind,
            message,
            ..
        } = notice
        {
            let body = if error_kind == "unauthorized" {
                "The stored key was rejected. Add a new one in Settings.".to_string()
            } else {
                // Anything else that leaves the credential unusable explains
                // itself better than one fixed sentence could.
                sentence(message)
            };

            show(format!("{display_name} refused the key"), body);
        }
    }
}

/// `ProviderError`'s messages follow an `error: ` prefix, so they start lower
/// case. A notification body is a sentence in its own right.
fn sentence(message: &str) -> String {
    let mut characters = message.chars();

    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => String::new(),
    }
}

#[tauri::command]
async fn refresh(app: AppHandle, only: Option<String>) -> Result<Vec<RefreshOutcome>, String> {
    refresh_and_store(&app, only.as_deref()).await
}

#[tauri::command]
fn snapshot_history(provider: String, limit: usize) -> Result<Vec<Snapshot>, String> {
    let connection = open_database().map_err(|error| error.to_string())?;
    history(&connection, &provider, limit).map_err(|error| error.to_string())
}

/// What happened when a key was offered for saving.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SaveKeyOutcome {
    provider: String,
    display_name: String,
    /// "saved_verified", "saved_unverified" or "rejected".
    status: String,
    /// The balance the candidate returned, when it could read one.
    balance: Option<f64>,
    /// Why it was rejected, or why no balance could be read.
    error_message: Option<String>,
}

/// Store an API key, but only after checking that it works.
///
/// All of the deciding happens in the core, so the CLI and this agree.
#[tauri::command]
async fn set_key(provider: String, key: String) -> Result<SaveKeyOutcome, String> {
    let display = display_name(&provider).to_string();
    let outcome = save_verified_key(&provider, &key)
        .await
        .map_err(|error| error.to_string())?;

    Ok(SaveKeyOutcome {
        provider,
        display_name: display,
        status: outcome.status().to_string(),
        balance: outcome.balance(),
        error_message: outcome.message().map(str::to_string),
    })
}

/// Drops the keychain entry. Stored readings stay, so this is reversible.
///
/// Returns the environment variable still supplying a key, if one is, so the
/// window can say why a provider it just removed is still on the list.
#[tauri::command]
fn remove_provider(provider: String) -> Result<Option<&'static str>, String> {
    forget_key(&provider).map_err(|error| error.to_string())
}

// ---------------------------------------------------------------------------
// Tray, poller, popover
// ---------------------------------------------------------------------------

const TRAY_ID: &str = "meterix-tray";
const DASHBOARD_LABEL: &str = "main";
const POPOVER_LABEL: &str = "tray";

/// Emitted once the poller has stored new readings, so an open window can
/// reload. The payload is deliberately empty: windows re-read the database
/// rather than being handed state, so there is one path for reading it.
const UPDATED_EVENT: &str = "balances-updated";

/// How often the poller wakes to see whether anything is due. Read fresh on every
/// beat, so changing an interval takes effect within half a minute.
const POLL_BEAT: Duration = Duration::from_secs(30);
const COLOUR_OK: [u8; 3] = [0x4D, 0xB6, 0xAC];
const COLOUR_LOW: [u8; 3] = [0xE0, 0xA6, 0x4B];
const COLOUR_ERROR: [u8; 3] = [0xD0, 0x8A, 0x5C];
const COLOUR_IDLE: [u8; 3] = [0x7A, 0x78, 0x71];

/// Draw the three-bar mark into an RGBA buffer.
///
/// Generated rather than shipped as files, so the colour can follow status
/// without four sets of icon assets to keep in step.
fn status_icon(colour: [u8; 3]) -> Image<'static> {
    const SIZE: u32 = 32;
    const BAR_HEIGHT: u32 = 5;
    // Widest first and descending, so it reads as a meter.
    const BARS: [(u32, u32); 3] = [(26, 4), (18, 13), (10, 22)];

    let mut pixels = vec![0u8; (SIZE * SIZE * 4) as usize];

    for (width, top) in BARS {
        let left = (SIZE - width) / 2;
        for y in top..(top + BAR_HEIGHT).min(SIZE) {
            for x in left..(left + width).min(SIZE) {
                let at = ((y * SIZE + x) * 4) as usize;
                pixels[at] = colour[0];
                pixels[at + 1] = colour[1];
                pixels[at + 2] = colour[2];
                pixels[at + 3] = 255;
            }
        }
    }

    Image::new_owned(pixels, SIZE, SIZE)
}

/// Every provider's effective threshold, as a map for the tray's colour check.
fn resolved_threshold_map(
    connection: &meterix_core::Connection,
) -> Result<HashMap<String, f64>, String> {
    Ok(meterix_core::resolved_thresholds(connection)
        .map_err(|error| error.to_string())?
        .into_iter()
        .collect())
}

/// The worst thing happening across the providers, which is what a tray icon
/// has room to say. One failing provider deserves more attention than another
/// one being healthy.
fn status_colour(outcomes: &[RefreshOutcome], thresholds: &HashMap<String, f64>) -> [u8; 3] {
    // A missing key is not something the icon should shout about. A fresh
    // install has no keys at all, and the dashboard already asks for one, so
    // colouring the tray an error would report a fault where there is only an
    // app nobody has set up yet. Same line the notifications draw: only a
    // rejected credential is worth interrupting someone over.
    let broken = outcomes.iter().any(|outcome| {
        !outcome.ok && outcome.error_kind.as_deref() != Some(MISSING_CREDENTIAL_KIND)
    });

    if broken {
        return COLOUR_ERROR;
    }

    let balances: Vec<(String, f64)> = outcomes
        .iter()
        .filter_map(|outcome| outcome.balance.map(|balance| (outcome.provider.clone(), balance)))
        .collect();

    if balances.is_empty() {
        return COLOUR_IDLE;
    }

    // A provider with no stored threshold is compared against nothing, so it
    // can only be healthy.
    let low = balances.iter().any(|(provider, balance)| {
        thresholds
            .get(provider)
            .is_some_and(|threshold| *balance < *threshold)
    });

    if low { COLOUR_LOW } else { COLOUR_OK }
}

fn update_tray(app: &AppHandle, outcomes: &[RefreshOutcome], thresholds: &HashMap<String, f64>) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };

    let _ = tray.set_icon(Some(status_icon(status_colour(outcomes, thresholds))));

    let total: f64 = outcomes.iter().filter_map(|outcome| outcome.balance).sum();
    let plural = if outcomes.len() == 1 { "" } else { "s" };
    let _ = tray.set_tooltip(Some(format!(
        "Meterix · {total:.2} across {} provider{plural}",
        outcomes.len()
    )));
}

/// Recolour the tray from the readings already on hand, against the thresholds as
/// they are now.
///
/// Called after a threshold is written. Nothing on the network has changed, so
/// the last batch of outcomes is still the right thing to compare — only the
/// comparison itself moved. Without this the icon kept its old colour until the
/// next poll, which can be half an hour away.
///
/// Returns quietly when nothing has been refreshed yet or the database cannot be
/// read: a stale icon is better than a failed settings save.
fn recolour_tray(app: &AppHandle) {
    let Some(cached) = app.try_state::<LastOutcomes>() else {
        return;
    };
    let Ok(rows) = cached.0.lock() else {
        return;
    };
    if rows.is_empty() {
        return;
    }

    let Ok(connection) = open_database() else {
        return;
    };
    let Ok(thresholds) = resolved_threshold_map(&connection) else {
        return;
    };

    update_tray(app, &rows, &thresholds);
}

fn show_dashboard(app: &AppHandle) {
    if let Some(popover) = app.get_webview_window(POPOVER_LABEL) {
        let _ = popover.hide();
    }

    if let Some(window) = app.get_webview_window(DASHBOARD_LABEL) {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Called from the popover. Done here rather than from the frontend so the
/// popover needs no window permissions of its own.
#[tauri::command]
fn open_dashboard(app: AppHandle) {
    show_dashboard(&app);
}

/// Show the popover beside the tray icon, or hide it if it is already up.
fn toggle_popover(app: &AppHandle, anchor: Option<tauri::Rect>) {
    let Some(popover) = popover_window(app) else {
        return;
    };

    if popover.is_visible().unwrap_or(false) {
        let _ = popover.hide();
        return;
    }

    if let Some(anchor) = anchor {
        place_popover(&popover, anchor);
    }

    let _ = popover.show();
    let _ = popover.set_focus();
}

/// The popover window, building it the first time one is asked for.
///
/// `create: false` in `tauri.conf.json` is what keeps it out of startup. A window
/// listed there is built straight away, so every session carried a second webview
/// that a session with no tray click never looked at. It is built from that same
/// config entry rather than from literals repeated here, so its size, chrome and
/// background stay in one place.
fn popover_window(app: &AppHandle) -> Option<tauri::WebviewWindow> {
    if let Some(existing) = app.get_webview_window(POPOVER_LABEL) {
        return Some(existing);
    }

    let config = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == POPOVER_LABEL)?;

    let popover = tauri::WebviewWindowBuilder::from_config(app, config)
        .and_then(|builder| builder.build())
        .map_err(|error| eprintln!("could not build the popover: {error}"))
        .ok()?;

    // Attached here because this is now the only place the window is built. The
    // popover should disappear as soon as it stops being the thing being used,
    // which is what every tray popover does.
    let handle = popover.clone();
    popover.on_window_event(move |event| {
        if let WindowEvent::Focused(false) = event {
            let _ = handle.hide();
        }
    });

    Some(popover)
}

/// Put the popover next to the tray icon, on whichever side has room. Windows
/// keeps the tray at the bottom of the screen, so in practice it ends up above.
fn place_popover(popover: &tauri::WebviewWindow, anchor: tauri::Rect) {
    let scale = popover.scale_factor().unwrap_or(1.0);
    let icon = anchor.position.to_physical::<f64>(scale);
    let icon_size = anchor.size.to_physical::<f64>(scale);

    let (Ok(Some(monitor)), Ok(size)) = (popover.current_monitor(), popover.outer_size()) else {
        return;
    };

    let width = f64::from(size.width);
    let height = f64::from(size.height);
    let screen = monitor.size();
    let origin = monitor.position();

    let under = icon.y + icon_size.height + 6.0;
    let y = if under + height <= f64::from(origin.y) + f64::from(screen.height) {
        under
    } else {
        icon.y - height - 6.0
    };

    let min_x = f64::from(origin.x) + 8.0;
    let max_x = (f64::from(origin.x) + f64::from(screen.width) - width - 8.0).max(min_x);
    let x = (icon.x + icon_size.width / 2.0 - width / 2.0).clamp(min_x, max_x);

    let _ = popover.set_position(tauri::PhysicalPosition::new(x, y));
}

/// Poll in the background, window open or not.
///
/// This is what makes the app a watcher rather than a viewer. The readings that
/// give the chart and the burn rate something to work with only accumulate if
/// something fetches unasked.
/// Take one look at what is due, and fetch those providers together.
async fn poll_due(app: &AppHandle) {
    let due = match open_database().and_then(|connection| due_providers(&connection)) {
        Ok(due) => due,
        Err(error) => {
            eprintln!("could not work out which providers are due: {error}");
            return;
        }
    };

    if due.is_empty() {
        return;
    }

    if let Err(error) = refresh_selected(app, &due).await {
        eprintln!("poll failed: {error}");
    }
}

fn spawn_poller(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            // Sleeping before looking: the dashboard refreshes on mount, so
            // fetching at launch would double the first round.
            tokio::time::sleep(POLL_BEAT).await;
            poll_due(&app).await;
        }
    });
}

fn build_tray(app: &AppHandle, quitting: Arc<AtomicBool>) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open dashboard", true, None::<&str>)?;
    let refresh = MenuItem::with_id(app, "refresh", "Refresh now", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Meterix", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &refresh, &quit])?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(status_icon(COLOUR_IDLE))
        .tooltip("Meterix")
        .menu(&menu)
        // Left click opens the popover; the menu is on right click.
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "open" => show_dashboard(app),
            "refresh" => {
                let app = app.clone();
                // Everything, not just what the poller thinks is due: that is
                // what pressing Refresh means.
                tauri::async_runtime::spawn(async move {
                    if let Err(error) = refresh_and_store(&app, None).await {
                        eprintln!("manual refresh failed: {error}");
                    }
                });
            }
            "quit" => {
                quitting.store(true, Ordering::SeqCst);
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = event
            {
                toggle_popover(tray.app_handle(), Some(rect));
            }
        })
        .build(app)?;

    Ok(())
}

/// Everything the settings screen needs, in one call.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsView {
    poll_interval_minutes: u32,
    low_balance_threshold: f64,
    notify_low_balance: bool,
    notify_key_errors: bool,
    /// The file the database is read from right now.
    database_path: String,
    /// The folder the database would be read from next launch. Different from
    /// `database_path`'s folder only between choosing a new one and restarting.
    data_directory: String,
    /// Where CSV exports are written, already resolved.
    export_directory: String,
    autostart_enabled: bool,
    providers: Vec<ProviderSetting>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProviderSetting {
    name: String,
    display_name: String,
    /// Null when this provider uses the app default.
    low_balance_threshold: Option<f64>,
    /// Never null: this list only holds providers that still have a key.
    key_hint: String,
    /// The number that applies when this provider's box is left blank: the
    /// provider's own published threshold, or the app default.
    fallback_threshold: f64,
    /// The earliest reading stored for this provider, so "tracking since" is a
    /// fact rather than a note taken when the row was first written. Null until
    /// it has a reading.
    first_reading_at: Option<String>,
    /// How often this provider alone is checked. Null when it uses the app-wide
    /// interval.
    poll_interval_minutes: Option<u32>,
}

#[tauri::command]
fn settings(app: AppHandle) -> Result<SettingsView, String> {
    use tauri_plugin_autostart::ManagerExt;

    let connection = open_database().map_err(|error| error.to_string())?;
    let current = load_settings(&connection).map_err(|error| error.to_string())?;
    let own = provider_thresholds(&connection).map_err(|error| error.to_string())?;
    let fallback = fallback_thresholds(&connection).map_err(|error| error.to_string())?;
    let started = first_reading_at(&connection).map_err(|error| error.to_string())?;
    let intervals = provider_intervals(&connection).map_err(|error| error.to_string())?;

    // Only providers that actually hold a key. Listing every supported provider
    // meant a fresh install showed two rows and two threshold boxes, which reads
    // as configuration that does not exist. A provider appears here once its key
    // has been saved from the dashboard.
    let providers = PROVIDERS
        .iter()
        .filter_map(|name| {
            Some(ProviderSetting {
                name: (*name).to_string(),
                display_name: display_name(name).to_string(),
                low_balance_threshold: own
                    .iter()
                    .find(|(provider, _)| provider == name)
                    .and_then(|(_, threshold)| *threshold),
                // What a blank box will actually mean for this provider, which is
                // not always the app default.
                fallback_threshold: fallback
                    .iter()
                    .find(|(provider, _)| provider == name)
                    .map_or(current.low_balance_threshold, |(_, value)| *value),
                first_reading_at: started
                    .iter()
                    .find(|(provider, _)| provider == name)
                    .and_then(|(_, value)| value.clone()),
                poll_interval_minutes: intervals
                    .iter()
                    .find(|(provider, _)| provider == name)
                    .and_then(|(_, minutes)| *minutes),
                key_hint: credential_hint(name)?,
            })
        })
        .collect::<Vec<_>>();

    // A path and a preference that can genuinely fail are reported as such
    // rather than defaulted, so the screen does not claim something it cannot
    // back up.
    Ok(SettingsView {
        poll_interval_minutes: current.poll_interval_minutes,
        low_balance_threshold: current.low_balance_threshold,
        notify_low_balance: current.notify_low_balance,
        notify_key_errors: current.notify_key_errors,
        database_path: meterix_core::database_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|error| format!("unavailable: {error}")),
        // Resolved from the anchor, so it is the folder the *next* launch will use
        // rather than the one the open connection came from. The two differ only
        // between choosing a new folder and restarting.
        data_directory: meterix_core::data_directory()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|error| format!("unavailable: {error}")),
        export_directory: export_directory(&current)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|error| format!("unavailable: {error}")),
        autostart_enabled: app.autolaunch().is_enabled().unwrap_or(false),
        providers,
    })
}

/// Saved as one form, because that is how the screen presents it.
/// Apply the wanted autostart state, touching the OS only when it differs.
///
/// Disabling something that was never enabled fails on Windows with "The system
/// cannot find the file specified". Without the comparison, anyone who leaves
/// autostart off would get that error on every unrelated settings change, because
/// the whole form is saved together and this is the one part of it that can fail.
fn apply_autostart(app: &AppHandle, desired: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;

    let manager = app.autolaunch();
    let enabled = manager.is_enabled().unwrap_or(false);

    if enabled == desired {
        return Ok(());
    }

    let changed = if desired {
        manager.enable()
    } else {
        manager.disable()
    };

    changed.map_err(|error| error.to_string())
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: Settings, autostart: bool) -> Result<(), String> {
    let connection = open_database().map_err(|error| error.to_string())?;
    persist_settings(&connection, &settings).map_err(|error| error.to_string())?;

    apply_autostart(&app, autostart)?;

    // The app-wide default threshold travels with this, so every provider sitting
    // on that default needs its colour worked out again.
    recolour_tray(&app);

    Ok(())
}

/// Null clears the override and puts the provider back on the app default.
#[tauri::command]
fn set_provider_threshold(
    app: AppHandle,
    provider: String,
    threshold: Option<f64>,
) -> Result<(), String> {
    {
        let connection = open_database().map_err(|error| error.to_string())?;
        store_threshold(&connection, &provider, threshold).map_err(|error| error.to_string())?;
    }

    recolour_tray(&app);

    Ok(())
}

/// Null puts the provider back on the app-wide interval.
#[tauri::command]
fn set_provider_interval(provider: String, minutes: Option<u32>) -> Result<(), String> {
    let connection = open_database().map_err(|error| error.to_string())?;

    store_interval(&connection, &provider, minutes).map_err(|error| error.to_string())
}

/// Write the reading history out as CSV, in the chosen folder.
///
/// Straight into a folder rather than through a save dialog: the setting says where
/// exports belong, and asking every time would make the setting pointless. The
/// window shows the path it wrote.
///
/// The file is derived from the database and can be written again at any time, so
/// exporting twice replaces it instead of leaving copies to accumulate.
#[tauri::command]
fn export_history(provider: Option<String>) -> Result<String, String> {
    let connection = open_database().map_err(|error| error.to_string())?;
    let settings = load_settings(&connection).map_err(|error| error.to_string())?;
    let csv = history_csv(&connection, provider.as_deref()).map_err(|error| error.to_string())?;

    let name = provider.as_deref().map_or_else(
        || "meterix-history.csv".to_string(),
        |provider| format!("meterix-{provider}-history.csv"),
    );

    let folder = export_directory(&settings).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let path = folder.join(name);

    std::fs::write(&path, csv)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;

    Ok(path.display().to_string())
}

/// Point the app at a different folder for the database.
///
/// Copies the database there and records the choice; it is read from the new
/// folder on the next launch, which is why the window says so. Moving the file
/// while a connection is open is how a database ends up half in each place.
///
/// Resolves with the new folder, or `None` if the picker was cancelled.
#[tauri::command]
fn set_data_directory(app: AppHandle) -> Result<Option<String>, String> {
    let Some(folder) = app.dialog().file().blocking_pick_folder() else {
        return Ok(None);
    };

    let folder = folder
        .into_path()
        .map_err(|error| format!("that folder cannot be used: {error}"))?;

    meterix_core::relocate_data_directory(&folder).map_err(|error| error.to_string())?;

    Ok(Some(folder.display().to_string()))
}

/// Choose where CSV exports are written. `None` if the picker was cancelled.
#[tauri::command]
fn set_export_directory(app: AppHandle) -> Result<Option<String>, String> {
    let Some(folder) = app.dialog().file().blocking_pick_folder() else {
        return Ok(None);
    };

    let folder = folder
        .into_path()
        .map_err(|error| format!("that folder cannot be used: {error}"))?;
    let chosen = folder.display().to_string();

    let connection = open_database().map_err(|error| error.to_string())?;
    let mut settings = load_settings(&connection).map_err(|error| error.to_string())?;
    settings.export_directory = Some(chosen.clone());
    persist_settings(&connection, &settings).map_err(|error| error.to_string())?;

    Ok(Some(chosen))
}

fn main() {
    let quitting = Arc::new(AtomicBool::new(false));
    let quit_flag = Arc::clone(&quitting);

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            overview,
            refresh,
            snapshot_history,
            set_key,
            remove_provider,
            open_dashboard,
            settings,
            save_settings,
            set_provider_threshold,
            set_provider_interval,
            export_history,
            set_data_directory,
            set_export_directory
        ])
        .setup(move |app| {
            // Before the poller or any window touches the database: an earlier build
            // kept it in a platform data directory, and this one looks in ~/.meterix.
            match adopt_legacy_database() {
                Ok(Some(previous)) => eprintln!(
                    "carried the database over from {}; the old copy is still there",
                    previous.display()
                ),
                Ok(None) => {}
                Err(error) => eprintln!("could not carry the database over: {error}"),
            }

            let handle = app.handle().clone();

            app.manage(LastOutcomes::default());
            app.manage(LastOutcomes::default());
            build_tray(&handle, Arc::clone(&quitting))?;

            // Closing the dashboard hides it rather than tearing down its
            // webview, so reopening it is instant and the poller's events still
            // have somewhere to land.
            if let Some(window) = app.get_webview_window(DASHBOARD_LABEL) {
                // A separate handle: `on_window_event` borrows the window, so
                // the closure cannot capture the same binding.
                let handle = window.clone();
                window.on_window_event(move |event| {
                    if let WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = handle.hide();
                    }
                });
            }

            // The popover is not built here: `create: false` leaves it to the
            // first tray click. See `popover_window`.

            spawn_poller(handle);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("the tauri application failed to start")
        .run(move |_app, event| {
            // With the windows closed the poller keeps running, which is the
            // point of a tray app. Only the tray menu's Quit lets it exit.
            if let tauri::RunEvent::ExitRequested { api, .. } = event
                && !quit_flag.load(Ordering::SeqCst)
            {
                api.prevent_exit();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider that answered.
    fn read(provider: &str, balance: f64) -> RefreshOutcome {
        RefreshOutcome {
            provider: provider.to_string(),
            display_name: provider.to_string(),
            ok: true,
            balance: Some(balance),
            basis: None,
            account_credits: None,
            usage: None,
            spend_window_days: None,
            error_kind: None,
            error_message: None,
        }
    }

    /// A provider that failed, with the identifier the core records for it.
    fn failed(provider: &str, kind: &str) -> RefreshOutcome {
        RefreshOutcome {
            provider: provider.to_string(),
            display_name: provider.to_string(),
            ok: false,
            balance: None,
            basis: None,
            account_credits: None,
            usage: None,
            spend_window_days: None,
            error_kind: Some(kind.to_string()),
            error_message: None,
        }
    }

    fn thresholds(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), *value))
            .collect()
    }

    #[test]
    fn a_missing_key_does_not_force_the_error_colour() {
        // What a fresh install looks like: no key, so nothing is wrong.
        let outcomes = [failed("openrouter", MISSING_CREDENTIAL_KIND)];
        assert_eq!(status_colour(&outcomes, &HashMap::new()), COLOUR_IDLE);
    }

    #[test]
    fn other_failures_do_force_it() {
        for kind in ["unauthorized", "forbidden", "rate_limited", "unreachable"] {
            let outcomes = [failed("openrouter", kind)];
            assert_eq!(
                status_colour(&outcomes, &HashMap::new()),
                COLOUR_ERROR,
                "{kind} should colour the tray as an error"
            );
        }
    }

    #[test]
    fn the_threshold_alone_decides_between_ok_and_low() {
        // The same reading judged two ways. This is exactly what changing a
        // threshold has to be able to move without fetching anything again.
        let outcomes = [read("openrouter", 6.40)];

        assert_eq!(
            status_colour(&outcomes, &thresholds(&[("openrouter", 5.00)])),
            COLOUR_OK
        );
        assert_eq!(
            status_colour(&outcomes, &thresholds(&[("openrouter", 50.0)])),
            COLOUR_LOW
        );
    }

    #[test]
    fn one_low_provider_is_enough() {
        let outcomes = [read("openrouter", 6.40), read("cheaperinference", 1.00)];
        let thresholds = thresholds(&[("openrouter", 5.00), ("cheaperinference", 2.00)]);
        assert_eq!(status_colour(&outcomes, &thresholds), COLOUR_LOW);
    }
}
