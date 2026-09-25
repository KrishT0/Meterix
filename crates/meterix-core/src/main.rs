//! Command line front end for the meterix core.
//!
//! Everything of substance lives in `lib.rs`; this file only parses arguments
//! and formats output. The Tauri app will be a third entry point over the same
//! library.

use std::env;

use anyhow::{Context, Result, anyhow};

use meterix_core::{
    Balance, PROVIDERS, SaveOutcome, adopt_legacy_database, display_name, fetch_balances, forget_key,
    history, history_csv, open_database, requested_providers, save_snapshot, save_verified_key,
};

const DEFAULT_HISTORY_LIMIT: usize = 20;

const USAGE: &str = "\
meterix-cli — how much credit is left with each LLM provider

usage
  meterix-cli [fetch [provider]]        read every provider, store a reading
  meterix-cli history <provider> [n]    the last n readings, 20 by default
  meterix-cli export [provider]         every reading as CSV, on standard output
  meterix-cli set-key <provider> <key>  verify a key, then store it
  meterix-cli forget-key <provider>     remove the stored key
  meterix-cli help                      show this

A key comes from the OS keychain first and the provider's environment variable
second, so the keychain always wins. `fetch` with no provider covers all of
them, and one failing does not stop the others.";

fn print_help() {
    println!("{USAGE}\n\nproviders\n  {}", PROVIDERS.join(", "));
}

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

    // A help flag anywhere answers with usage. Without this, `fetch --help`
    // reads "--help" as a provider name and reports it as unknown.
    if args.iter().skip(1).any(|arg| arg == "-h" || arg == "--help") {
        print_help();
        return Ok(());
    }

    // Before anything opens the database: an earlier build kept it in a platform
    // data directory, and this one looks in ~/.meterix.
    if let Some(previous) = adopt_legacy_database()? {
        eprintln!(
            "carried the database over from {}. The old copy is still there.",
            previous.display()
        );
    }

    let command = args.get(1).map(String::as_str).unwrap_or("fetch");

    match command {
        "help" => {
            print_help();
            Ok(())
        }
        "set-key" => {
            let provider = args
                .get(2)
                .context("usage: meterix-cli set-key <provider> <key>")?;
            let key = args
                .get(3)
                .context("usage: meterix-cli set-key <provider> <key>")?;

            set_key(provider, key).await
        }
        "export" => {
            let only = args.get(2).map(String::as_str);

            // Checked rather than filtered: a typo would otherwise write a file
            // with a header and nothing under it, which reads as "this provider
            // has no history" rather than as a mistake.
            if let Some(name) = only {
                requested_providers(Some(name))?;
            }

            let connection = open_database()?;
            // Standard output, so `export | something` works. The chosen folder
            // belongs to the window, which is the thing that writes a file.
            print!("{}", history_csv(&connection, only)?);

            Ok(())
        }
        "fetch" => fetch(args.get(2).map(String::as_str)).await,
        "forget-key" => {
            let provider = args
                .get(2)
                .context("usage: meterix-cli forget-key <provider>")?;

            match forget_key(provider)? {
                Some(env_name) => println!(
                    "key removed from the OS keychain, but {env_name} is set and still supplies \
                     one, so {} stays configured. Unset it to finish the job: the app cannot \
                     change the environment it was launched from.",
                    display_name(provider)
                ),
                None => println!("key removed from the OS keychain"),
            }

            Ok(())
        }
        "history" => {
            let provider = args
                .get(2)
                .context("usage: meterix-cli history <provider> [limit]")?;
            let limit = match args.get(3) {
                Some(raw) => raw.parse().context("limit must be a whole number")?,
                None => DEFAULT_HISTORY_LIMIT,
            };

            show_history(provider, limit)
        }
        _ => Err(anyhow!(
            "unknown command: {command}\nrun `meterix-cli --help` for the commands and providers"
        )),
    }
}
