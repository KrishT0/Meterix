//! Tauri shell over `meterix-core`.
//!
//! Nothing clever lives here. The commands translate between the core's types
//! and JSON, and the frontend does the presenting. Anything this file starts
//! doing on its own is a sign it belongs in the library instead.

// Release builds should not open a console window behind the app. Debug keeps
// it, because that is where Rust errors and panics show up.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, WindowEvent};

use meterix_core::{
    Basis, PROVIDERS, Snapshot, credential_hint, display_name, fetch_balances, forget_key, history,
    open_database, save_snapshot, save_verified_key,
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
    let mut rows = Vec::with_capacity(outcomes.len());

    for (provider, result) in outcomes {
        match result {
            Ok(balance) => {
                save_snapshot(&connection, &balance).map_err(|error| error.to_string())?;

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

    update_tray(app, &rows);

    // Tell every open window, so the popover and the dashboard cannot disagree.
    // They re-read the database rather than being handed this payload, which
    // keeps one path for reading state.
    let _ = app.emit(UPDATED_EVENT, ());

    Ok(rows)
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

/// How often the background poller runs.
///
/// ponytail: a fixed 30 minutes, the roadmap's default. Making it configurable
/// needs a settings column and a settings screen.
const POLL_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Balance below which a provider counts as low, for the tray colour.
///
/// Must match `LOW_BALANCE_THRESHOLD` in app/src/components/ui.tsx. It is
/// duplicated because the tray has to work with no window open, while the
/// dashboard tints its own cards. Both disappear when thresholds become
/// per-provider data, which is what v3 wants them to be.
const LOW_BALANCE_THRESHOLD: f64 = 2.0;

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

/// The worst thing happening across the providers, which is what a tray icon
/// has room to say. One failing provider deserves more attention than another
/// one being healthy.
fn status_colour(outcomes: &[RefreshOutcome]) -> [u8; 3] {
    if outcomes.iter().any(|outcome| !outcome.ok) {
        return COLOUR_ERROR;
    }

    let balances: Vec<f64> = outcomes
        .iter()
        .filter_map(|outcome| outcome.balance)
        .collect();

    if balances.is_empty() {
        return COLOUR_IDLE;
    }
    if balances.iter().any(|balance| *balance < LOW_BALANCE_THRESHOLD) {
        return COLOUR_LOW;
    }

    COLOUR_OK
}

fn update_tray(app: &AppHandle, outcomes: &[RefreshOutcome]) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };

    let _ = tray.set_icon(Some(status_icon(status_colour(outcomes))));

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

fn spawn_poller(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            // Sleep before the first poll: the dashboard already refreshes on
            // mount, so polling straight away would double every launch.
            tokio::time::sleep(POLL_INTERVAL).await;
            poll_once(&app).await;
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

fn main() {
    let quitting = Arc::new(AtomicBool::new(false));
    let quit_flag = Arc::clone(&quitting);

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            overview,
            refresh,
            snapshot_history,
            set_key,
            remove_provider,
            open_dashboard
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

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
