//! Tauri shell over `meterix-core`.
//!
//! Nothing clever lives here. The commands translate between the core's types
//! and JSON, and the frontend does the presenting. Anything this file starts
//! doing on its own is a sign it belongs in the library instead.

// Release builds should not open a console window behind the app. Debug keeps
// it, because that is where Rust errors and panics show up.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tauri_plugin_notification::NotificationExt;

use meterix_core::{
    Basis, DEFAULT_POLL_INTERVAL_MINUTES, Notice, PROVIDERS, Settings, Snapshot, credential_hint,
    display_name, effective_threshold, fetch_balances, forget_key, history, load_settings,
    open_database, provider_fingerprint, provider_thresholds, save_settings as persist_settings,
    save_snapshot, save_verified_key, set_provider_threshold as store_threshold,
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
#[derive(Serialize)]
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

#[tauri::command]
fn overview() -> Result<Vec<ProviderOverview>, String> {
    let connection = open_database().map_err(|error| error.to_string())?;
    let settings = load_settings(&connection).map_err(|error| error.to_string())?;
    let own = provider_thresholds(&connection).map_err(|error| error.to_string())?;

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

            let override_threshold = own
                .iter()
                .find(|(provider, _)| provider == name)
                .and_then(|(_, threshold)| *threshold);

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
                threshold: effective_threshold(&settings, override_threshold),
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
    // Fetch first and open the database afterwards. A rusqlite `Connection` is
    // Send but not Sync, so holding one across an await would make this future
    // non-Send and stop the command from compiling.
    let outcomes = fetch_balances(only).await.map_err(|error| error.to_string())?;

    let connection = open_database().map_err(|error| error.to_string())?;
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
#[tauri::command]
fn remove_provider(provider: String) -> Result<(), String> {
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

/// How often the background poller runs, until someone changes it. The stored
/// setting is read fresh on every loop; `Poller::wake` cuts the current wait
/// short when it changes.
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
    if outcomes.iter().any(|outcome| !outcome.ok) {
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
    let Some(popover) = app.get_webview_window(POPOVER_LABEL) else {
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
async fn poll_once(app: &AppHandle) {
    if let Err(error) = refresh_and_store(app, None).await {
        eprintln!("poll failed: {error}");
    }
}

/// Lets a settings change cut the poller's sleep short, so a new interval takes
/// effect immediately rather than after the old one has elapsed.
#[derive(Default)]
struct Poller {
    wake: tokio::sync::Notify,
}

fn stored_interval_minutes() -> u32 {
    open_database()
        .and_then(|connection| load_settings(&connection))
        .map(|settings| settings.poll_interval_minutes)
        .unwrap_or(DEFAULT_POLL_INTERVAL_MINUTES)
}

fn spawn_poller(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let minutes = stored_interval_minutes();
            let wait = tokio::time::sleep(Duration::from_secs(u64::from(minutes) * 60));

            // Bound before the macro: `app.state` returns a temporary, and
            // borrowing it inline would drop it mid-expression.
            let poller = app.state::<Poller>();

            // Sleeping before the first poll: the dashboard already refreshes
            // on mount, so polling straight away would double every launch.
            tokio::select! {
                () = wait => poll_once(&app).await,
                // The interval changed. Go round and read the new one.
                () = poller.wake.notified() => {}
            }
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
                tauri::async_runtime::spawn(async move { poll_once(&app).await });
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
    database_path: String,
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
    key_hint: Option<String>,
}

#[tauri::command]
fn settings(app: AppHandle) -> Result<SettingsView, String> {
    use tauri_plugin_autostart::ManagerExt;

    let connection = open_database().map_err(|error| error.to_string())?;
    let current = load_settings(&connection).map_err(|error| error.to_string())?;
    let own = provider_thresholds(&connection).map_err(|error| error.to_string())?;

    let providers = PROVIDERS
        .iter()
        .map(|name| ProviderSetting {
            name: (*name).to_string(),
            display_name: display_name(name).to_string(),
            low_balance_threshold: own
                .iter()
                .find(|(provider, _)| provider == name)
                .and_then(|(_, threshold)| *threshold),
            key_hint: credential_hint(name),
        })
        .collect();

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

    // Cut the current sleep short, so a new interval is not waiting behind the
    // old one.
    app.state::<Poller>().wake.notify_one();

    Ok(())
}

/// Null clears the override and puts the provider back on the app default.
#[tauri::command]
fn set_provider_threshold(provider: String, threshold: Option<f64>) -> Result<(), String> {
    let connection = open_database().map_err(|error| error.to_string())?;
    store_threshold(&connection, &provider, threshold).map_err(|error| error.to_string())
}

fn main() {
    let quitting = Arc::new(AtomicBool::new(false));
    let quit_flag = Arc::clone(&quitting);

    tauri::Builder::default()
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
            set_provider_threshold
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            app.manage(Poller::default());
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

            // The popover should disappear as soon as it stops being the thing
            // being used, which is what every tray popover does.
            if let Some(popover) = app.get_webview_window(POPOVER_LABEL) {
                let handle = popover.clone();
                popover.on_window_event(move |event| {
                    if let WindowEvent::Focused(false) = event {
                        let _ = handle.hide();
                    }
                });
            }

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
