//! Command line front end for the meterix core.
//!
//! Everything of substance lives in `lib.rs`; this file only parses arguments
//! and formats output. The Tauri app will be a third entry point over the same
//! library.

use std::env;

use anyhow::{Context, Result, anyhow};

use meterix_core::{
    Balance, PROVIDERS, fetch_balances, history, open_database, save_key, save_snapshot,
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
                    name,
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

fn show_history(provider: &str, limit: usize) -> Result<()> {
    let connection = open_database()?;

    println!(
        "{:<19}  {:<15}  {:>17}  {:>9}  {:>12}",
        "recorded_at", "basis", "account credits", "usage", "remaining"
    );

    for snapshot in history(&connection, provider, limit)? {
        println!(
            "{:<19}  {:<15}  {:>17}  {:>9}  {:>12}",
            snapshot.recorded_at,
            snapshot.basis.as_str(),
            money(snapshot.account_credits),
            money(snapshot.usage),
            format!("${:.2}", snapshot.remaining)
        );
    }

    Ok(())
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

            save_key(provider, key)?;
            println!("key saved to the OS keychain");
            Ok(())
        }
        "fetch" => fetch(args.get(2).map(String::as_str)).await,
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
             usage: meterix-core [fetch [provider] | history <provider> [limit] | set-key <provider> <key>]\n\
             providers: {}",
            PROVIDERS.join(", ")
        )),
    }
}
