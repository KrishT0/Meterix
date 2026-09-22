//! Command line front end for the meterix core.
//!
//! Everything of substance lives in `lib.rs`; this file only parses arguments
//! and formats output. The Tauri app will be a third entry point over the same
//! library.

use std::env;

use anyhow::{Context, Result, anyhow};

use meterix_core::{
    Balance, PROVIDERS, SaveOutcome, display_name, fetch_balances, forget_key, history,
    open_database, save_snapshot, save_verified_key,
};

const DEFAULT_HISTORY_LIMIT: usize = 20;

fn money(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_string(), |value| format!("${value:.2}"))
}

/// The number plus what it means, because a value is not always a balance.
fn amount(balance: &Balance) -> String {
    if balance.basis.is_balance() {
        format!("${:.2}", balance.remaining)
    } else {
        format!("${:.2} used", balance.remaining)
    }
}

async fn fetch(only: Option<&str>) -> Result<()> {
    let connection = open_database()?;
    let outcomes = fetch_balances(only).await?;

    let mut saved = 0usize;
    let mut failed: Vec<&str> = Vec::new();

    for (name, outcome) in &outcomes {
        match outcome {
            Ok(balance) => {
                println!(
                    "{:<17}  {:>12}  {}",
                    display_name(name),
                    amount(balance),
                    balance.basis.label()
                );
                save_snapshot(&connection, balance)?;
                saved += 1;
            }
            Err(error) => {
                eprintln!("{name}: {error}");
                failed.push(name);
            }
        }
    }

    if saved == 0 {
        return Err(anyhow!("every provider failed: {}", failed.join(", ")));
    }

    println!("(saved {saved} snapshots)");
    Ok(())
}

/// Check a key before storing it, so a typo cannot destroy a working one.
async fn set_key(provider: &str, key: &str) -> Result<()> {
    let name = display_name(provider);

    match save_verified_key(provider, key).await? {
        SaveOutcome::Verified(balance) => {
            println!(
                "key verified and stored · {name} reports {}",
                amount(&balance)
            );
        }
        SaveOutcome::SavedUnverified(reason) => {
            println!("key stored, but no balance could be read · {reason}");
        }
        SaveOutcome::Rejected(reason) => {
            // Non-zero so a script notices, and the reason the provider gave.
            return Err(anyhow!(
                "not saved · {name} refused this key. {reason} The stored key is unchanged."
            ));
        }
    }

    Ok(())
}

fn show_history(provider: &str, limit: usize) -> Result<()> {
    let connection = open_database()?;

    println!(
        "{:<19}  {:<15}  {:>17}  {:>9}  {:>7}  {:>12}",
        "recorded_at", "basis", "account credits", "usage", "window", "remaining"
    );

    for snapshot in history(&connection, provider, limit)? {
        println!(
            "{:<19}  {:<15}  {:>17}  {:>9}  {:>7}  {:>12}",
            snapshot.recorded_at,
            snapshot.basis.as_str(),
            money(snapshot.account_credits),
            money(snapshot.usage),
            // "all" and "90d" are not the same kind of number.
            window(snapshot.spend_window_days),
            format!("${:.2}", snapshot.remaining)
        );
    }

    Ok(())
}

fn window(days: Option<u32>) -> String {
    days.map_or_else(|| "all".to_string(), |days| format!("{days}d"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    let command = args.get(1).map(String::as_str).unwrap_or("fetch");

    match command {
        "set-key" => {
            let provider = args
                .get(2)
                .context("usage: meterix-core set-key <provider> <key>")?;
            let key = args
                .get(3)
                .context("usage: meterix-core set-key <provider> <key>")?;

            set_key(provider, key).await
        }
        "fetch" => fetch(args.get(2).map(String::as_str)).await,
        "forget-key" => {
            let provider = args
                .get(2)
                .context("usage: meterix-core forget-key <provider>")?;

            forget_key(provider)?;
            println!("key removed from the OS keychain");
            Ok(())
        }
        "history" => {
            let provider = args
                .get(2)
                .context("usage: meterix-core history <provider> [limit]")?;
            let limit = match args.get(3) {
                Some(raw) => raw.parse().context("limit must be a whole number")?,
                None => DEFAULT_HISTORY_LIMIT,
            };

            show_history(provider, limit)
        }
        _ => Err(anyhow!(
            "unknown command: {command}\n\
             usage: meterix-core [fetch [provider] | history <provider> [limit] | set-key <provider> <key> | forget-key <provider>]\n\
             providers: {}",
            PROVIDERS.join(", ")
        )),
    }
}
