//! Tauri shell over `meterix-core`.
//!
//! Nothing clever lives here. The commands translate between the core's types
//! and JSON, and the frontend does the presenting. Anything this file starts
//! doing on its own is a sign it belongs in the library instead.

// Release builds should not open a console window behind the app. Debug keeps
// it, because that is where Rust errors and panics show up.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::Serialize;

use meterix_core::{
    Basis, PROVIDERS, Snapshot, credential_hint, display_name, fetch_balances, forget_key, history,
    open_database, save_key, save_snapshot,
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
async fn refresh(only: Option<String>) -> Result<Vec<RefreshOutcome>, String> {
    // Fetch first and open the database afterwards. A rusqlite `Connection` is
    // Send but not Sync, so holding one across an await would make this future
    // non-Send and stop the command from compiling.
    let outcomes = fetch_balances(only.as_deref())
        .await
        .map_err(|error| error.to_string())?;

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

    Ok(rows)
}

#[tauri::command]
fn snapshot_history(provider: String, limit: usize) -> Result<Vec<Snapshot>, String> {
    let connection = open_database().map_err(|error| error.to_string())?;
    history(&connection, &provider, limit).map_err(|error| error.to_string())
}

#[tauri::command]
fn set_key(provider: String, key: String) -> Result<(), String> {
    save_key(&provider, &key).map_err(|error| error.to_string())
}

/// Drops the keychain entry. Stored readings stay, so this is reversible.
#[tauri::command]
fn remove_provider(provider: String) -> Result<(), String> {
    forget_key(&provider).map_err(|error| error.to_string())
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            overview,
            refresh,
            snapshot_history,
            set_key,
            remove_provider
        ])
        .run(tauri::generate_context!())
        .expect("the tauri application failed to start");
}
