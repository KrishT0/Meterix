//! Command line front end for the meterix core.
//!
//! Everything of substance lives in `lib.rs`; this file only parses arguments
//! and formats output. The Tauri app will be a third entry point over the same
//! library.

use std::env;

use anyhow::{Context, Result, anyhow};

use std::collections::HashMap;

use meterix_core::{
    Balance, Connection, GATEWAY_SHAPES, PROVIDERS, SaveOutcome, add_gateway, adopt_legacy_database,
    display_name, fetch_selected, first_reading_at, forget_key, gateway_for, history, history_csv, open_database,
    provider_enabled, reading_counts, requested_providers, resolved_intervals, resolved_thresholds,
    save_snapshot, save_verified_key, tracked_providers,
};

const DEFAULT_HISTORY_LIMIT: usize = 20;

const USAGE: &str = "\
meterix-cli — how much credit is left with each LLM provider

usage
  meterix-cli [fetch [provider]]        read every provider, store a reading
  meterix-cli history <provider> [n]    the last n readings, 20 by default
  meterix-cli export [provider]         every reading as CSV, on standard output
  meterix-cli providers                 what is tracked, and what is stored
  meterix-cli add-gateway <name> --shape <shape> --base-url <url>
                                        track a provider reached through a gateway
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
    // Worked out before the await: a rusqlite connection held across one makes the
    // future non-Send. Naming a provider explicitly fetches it whether or not it is
    // tracked, since that is a deliberate act; naming none means everything being
    // watched, because a switched-off provider is one the app has said to leave
    // alone.
    let selection = {
        let connection = open_database()?;

        match only {
            Some(name) => requested_providers(Some(name))?,
            None => tracked_providers(&connection)?,
        }
    };

    let connection = open_database()?;
    let outcomes = fetch_selected(&selection).await?;

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

/// The value after a `--flag`, which is all the argument parsing this needs.
fn flag(args: &[String], name: &str) -> Option<String> {
    let position = args.iter().position(|argument| argument == name)?;

    args.get(position + 1).cloned()
}

fn window(days: Option<u32>) -> String {
    days.map_or_else(|| "all".to_string(), |days| format!("{days}d"))
}

/// What the app knows about each provider, and which ones it is watching.
///
/// `readings` is here because switching a provider off keeps its history, and a
/// number going down would be the first sign that it did not.
fn show_providers(connection: &Connection) -> Result<()> {
    let thresholds: HashMap<String, f64> = resolved_thresholds(connection)?.into_iter().collect();
    let intervals: HashMap<String, u32> = resolved_intervals(connection)?.into_iter().collect();
    let started: HashMap<String, Option<String>> =
        first_reading_at(connection)?.into_iter().collect();
    let readings: HashMap<String, i64> = reading_counts(connection)?.into_iter().collect();

    println!(
        "{:<18} {:<8} {:>8} {:>7} {:>7} {:>9}",
        "provider", "tracked", "low at", "every", "since", "readings"
    );

    for (name, enabled) in provider_enabled(connection)? {
        // A switched-off provider has no resolved threshold or interval, because
        // those are only worked out for what is being watched.
        let low = thresholds
            .get(&name)
            .map_or_else(|| "-".to_string(), |value| format!("${value:.2}"));
        let every = intervals
            .get(&name)
            .map_or_else(|| "-".to_string(), |minutes| format!("{minutes}m"));
        let since = started
            .get(&name)
            .and_then(Option::as_deref)
            .map_or_else(|| "-".to_string(), |at| at.get(..10).unwrap_or(at).to_string());

        println!(
            "{:<18} {:<8} {:>8} {:>7} {:>7} {:>9}",
            name,
            if enabled { "yes" } else { "no" },
            low,
            every,
            since,
            readings.get(&name).copied().unwrap_or(0)
        );
    }

    Ok(())
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
        "add-gateway" => {
            let name = args.get(2).context(
                "usage: meterix-cli add-gateway <name> --shape <shape> --base-url <url> \
                 [--display-name <label>]",
            )?;
            let shape = flag(&args, "--shape").with_context(|| {
                format!("--shape is required; this build has {}", GATEWAY_SHAPES.join(", "))
            })?;
            let base_url = flag(&args, "--base-url").context("--base-url is required")?;
            let display_name = flag(&args, "--display-name");

            let connection = open_database()?;

            add_gateway(
                &connection,
                name,
                &shape,
                &base_url,
                display_name.as_deref(),
            )?;

            // Read back rather than echoing the argument: the base url is
            // normalised on the way in, so quoting the input would claim a trailing
            // slash the row does not have.
            match gateway_for(&connection, name)? {
                Some(gateway) => {
                    println!("{name} added: {} shape at {}", gateway.shape, gateway.base_url)
                }
                None => println!("{name} added"),
            }
            println!("give it a key with: meterix-cli set-key {name} <key>");

            Ok(())
        }
        "providers" => {
            let connection = open_database()?;

            show_providers(&connection)
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
            let connection = open_database()?;

            match forget_key(&connection, provider)? {
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
