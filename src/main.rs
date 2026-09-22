use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use keyring::Entry;
use reqwest::Client;
use rusqlite::{Connection, params};
use serde::Deserialize;
use std::env;

const DATABASE_PATH: &str = "meterix.db";
const KEYRING_SERVICE: &str = "meterix-core";
const PROVIDERS: [&str; 2] = ["openrouter", "cheaperinference"];

#[derive(Debug, Clone)]
struct Balance {
    provider: &'static str,
    remaining: f64,
    /// Account balance from the provider's credits endpoint, when readable.
    account_credits: Option<f64>,
    /// Spend so far on this credential, when the provider reports it.
    usage: Option<f64>,
}

/// One stored row of `balance_snapshots`.
struct Snapshot {
    recorded_at: String,
    account_credits: Option<f64>,
    usage: Option<f64>,
    remaining: f64,
}

fn money(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_string(), |value| format!("${value:.2}"))
}

#[async_trait]
trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn fetch_balance(&self) -> Result<Balance>;
}

struct OpenRouter {
    client: Client,
    key: String,
}

const OPENROUTER_KEY_URL: &str = "https://openrouter.ai/api/v1/key";
const OPENROUTER_CREDITS_URL: &str = "https://openrouter.ai/api/v1/credits";

#[derive(Debug, Deserialize)]
struct OpenRouterResponse {
    data: OpenRouterData,
}

#[derive(Debug, Deserialize)]
struct OpenRouterData {
    /// Per-key spending cap. `None` means no cap was configured, which is the
    /// normal state for a personal key — not a malformed response.
    limit_remaining: Option<f64>,
    /// Total USD spent on this key. Present on any valid key.
    usage: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterCreditsResponse {
    data: OpenRouterCredits,
}

#[derive(Debug, Deserialize)]
struct OpenRouterCredits {
    total_credits: f64,
    total_usage: f64,
}

impl OpenRouter {
    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.key)
            .send()
            .await
            .with_context(|| format!("request to {url} failed"))?;

        response
            .error_for_status()
            .with_context(|| format!("{url} returned an error"))?
            .json::<T>()
            .await
            .with_context(|| format!("invalid response from {url}"))
    }
}

#[async_trait]
impl Provider for OpenRouter {
    fn name(&self) -> &'static str {
        "openrouter"
    }

    async fn fetch_balance(&self) -> Result<Balance> {
        let key_info = self
            .get_json::<OpenRouterResponse>(OPENROUTER_KEY_URL)
            .await?
            .data;

        // Account credits are the real balance, but reading them requires a
        // management key, so a failure here is expected rather than fatal.
        let account_credits = self
            .get_json::<OpenRouterCreditsResponse>(OPENROUTER_CREDITS_URL)
            .await
            .ok()
            .map(|credits| credits.data.total_credits - credits.data.total_usage);

        let remaining = account_credits
            .or(key_info.limit_remaining)
            .or(key_info.usage)
            .ok_or_else(|| {
                anyhow!("OpenRouter returned neither account credits, a key cap, nor usage")
            })?;

        if account_credits.is_none() && key_info.limit_remaining.is_none() {
            eprintln!(
                "openrouter: no management-key access to account credits and no spending cap on \
                 this key; the recorded remaining value is usage so far, not a balance"
            );
        }

        Ok(Balance {
            provider: self.name(),
            remaining,
            account_credits,
            usage: key_info.usage,
        })
    }
}

struct CheaperInference {
    client: Client,
    key: String,
}

#[derive(Debug, Deserialize)]
struct CheaperInferenceResponse {
    available_usd: f64,
}

#[async_trait]
impl Provider for CheaperInference {
    fn name(&self) -> &'static str {
        "cheaperinference"
    }

    async fn fetch_balance(&self) -> Result<Balance> {
        let response = self
            .client
            .get("https://api.cheaperinference.com/v1/account/balance")
            .bearer_auth(&self.key)
            .send()
            .await
            .context("request to CheaperInference failed")?;

        let response = response
            .error_for_status()
            .context("CheaperInference returned an error")?
            .json::<CheaperInferenceResponse>()
            .await
            .context("invalid CheaperInference response")?;

        Ok(Balance {
            provider: self.name(),
            remaining: response.available_usd,
            account_credits: Some(response.available_usd),
            usage: None,
        })
    }
}

fn credential(provider: &str, env_name: &str) -> Result<String> {
    if let Ok(entry) = Entry::new(KEYRING_SERVICE, provider) {
        if let Ok(key) = entry.get_password() {
            return Ok(key);
        }
    }

    env::var(env_name).with_context(|| {
        format!(
            "missing {env_name}; run `set-key {provider} <key>` or set the environment variable"
        )
    })
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement.query_map([], |row| row.get::<_, String>(1))?;

    for name in names {
        if name? == column {
            return Ok(true);
        }
    }

    Ok(false)
}

fn initialize_database(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS providers (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE
        );

        CREATE TABLE IF NOT EXISTS balance_snapshots (
            id INTEGER PRIMARY KEY,
            provider_id INTEGER NOT NULL REFERENCES providers(id),
            remaining REAL NOT NULL,
            account_credits REAL,
            usage REAL,
            recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        ",
    )?;

    // Databases written before these columns existed need them added; SQLite
    // has no ADD COLUMN IF NOT EXISTS.
    for column in ["account_credits", "usage"] {
        if !column_exists(connection, "balance_snapshots", column)? {
            connection.execute(
                &format!("ALTER TABLE balance_snapshots ADD COLUMN {column} REAL"),
                [],
            )?;
        }
    }

    for provider in PROVIDERS {
        connection.execute(
            "INSERT OR IGNORE INTO providers (name) VALUES (?1)",
            params![provider],
        )?;
    }

    Ok(())
}

fn save_snapshot(connection: &Connection, balance: &Balance) -> Result<()> {
    connection.execute(
        "
        INSERT INTO balance_snapshots (provider_id, remaining, account_credits, usage)
        SELECT id, ?1, ?2, ?3 FROM providers WHERE name = ?4
        ",
        params![
            balance.remaining,
            balance.account_credits,
            balance.usage,
            balance.provider
        ],
    )?;
    Ok(())
}

fn history(connection: &Connection, provider: &str, limit: usize) -> Result<Vec<Snapshot>> {
    let limit = i64::try_from(limit).context("history limit is too large")?;
    let mut statement = connection.prepare(
        "
        SELECT balance_snapshots.recorded_at,
               balance_snapshots.account_credits,
               balance_snapshots.usage,
               balance_snapshots.remaining
        FROM balance_snapshots
        JOIN providers ON providers.id = balance_snapshots.provider_id
        WHERE providers.name = ?1
        ORDER BY balance_snapshots.recorded_at DESC, balance_snapshots.id DESC
        LIMIT ?2
        ",
    )?;

    let rows = statement.query_map(params![provider, limit], |row| {
        Ok(Snapshot {
            recorded_at: row.get(0)?,
            account_credits: row.get(1)?,
            usage: row.get(2)?,
            remaining: row.get(3)?,
        })
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not read balance history")
}

fn build_provider(name: &str, client: &Client) -> Result<Box<dyn Provider>> {
    let provider: Box<dyn Provider> = match name {
        "openrouter" => Box::new(OpenRouter {
            client: client.clone(),
            key: credential("openrouter", "OPENROUTER_KEY")?,
        }),
        "cheaperinference" => Box::new(CheaperInference {
            client: client.clone(),
            key: credential("cheaperinference", "CHEAPERINFERENCE_KEY")?,
        }),
        _ => {
            return Err(anyhow!(
                "unknown provider: {name}; use {}",
                PROVIDERS.join(", ")
            ));
        }
    };

    Ok(provider)
}

async fn fetch_balances(only: Option<&str>) -> Result<Vec<Balance>> {
    if let Some(name) = only
        && !PROVIDERS.contains(&name)
    {
        return Err(anyhow!(
            "unknown provider: {name}; use {}",
            PROVIDERS.join(", ")
        ));
    }

    let client = Client::new();
    let names: Vec<&str> = match only {
        Some(name) => vec![name],
        None => PROVIDERS.to_vec(),
    };

    let mut balances = Vec::with_capacity(names.len());
    let mut failures = Vec::new();

    for name in names {
        let outcome: Result<Balance> = async {
            let provider = build_provider(name, &client)?;
            provider.fetch_balance().await
        }
        .await;

        match outcome {
            Ok(balance) => balances.push(balance),
            Err(error) => {
                eprintln!("{name}: {error:#}");
                failures.push(name);
            }
        }
    }

    if balances.is_empty() {
        return Err(anyhow!("all providers failed: {}", failures.join(", ")));
    }

    Ok(balances)
}

fn save_key(provider: &str, key: &str) -> Result<()> {
    if !PROVIDERS.contains(&provider) {
        return Err(anyhow!(
            "unknown provider: {provider}; use {}",
            PROVIDERS.join(", ")
        ));
    }

    Entry::new(KEYRING_SERVICE, provider)
        .context("could not access the OS keychain")?
        .set_password(key)
        .context("could not save key to the OS keychain")?;
    println!("key saved to keychain");
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
            save_key(provider, key)
        }
        "fetch" => {
            let only = args.get(2).map(String::as_str);
            let connection = Connection::open(DATABASE_PATH)
                .with_context(|| format!("could not open {DATABASE_PATH}"))?;
            initialize_database(&connection)?;

            let balances = fetch_balances(only).await?;
            for balance in &balances {
                println!(
                    "{:<17} ${:.2} remaining",
                    balance.provider, balance.remaining
                );
                save_snapshot(&connection, balance)?;
            }
            println!("(saved {} snapshots to {})", balances.len(), DATABASE_PATH);
            Ok(())
        }
        "history" => {
            let provider = args
                .get(2)
                .context("usage: meterix-core history <provider>")?;
            let connection = Connection::open(DATABASE_PATH)
                .with_context(|| format!("could not open {DATABASE_PATH}"))?;
            initialize_database(&connection)?;

            println!(
                "{:<19}  {:>17}  {:>9}  {:>11}",
                "recorded_at", "account credits", "usage", "remaining"
            );

            for snapshot in history(&connection, provider, 20)? {
                println!(
                    "{:<19}  {:>17}  {:>9}  {:>11}",
                    snapshot.recorded_at,
                    money(snapshot.account_credits),
                    money(snapshot.usage),
                    format!("${:.2}", snapshot.remaining)
                );
            }
            Ok(())
        }
        _ => Err(anyhow!(
            "unknown command: {command}; use fetch [provider], history <provider>, or set-key <provider> <key>"
        )),
    }
}
