//! Core of Meterix: read AI provider credit balances and keep them in SQLite.
//!
//! There is no `main` here on purpose. The CLI (`src/main.rs`) and the future
//! Tauri app both link this crate, and Tauri owns its own argv and its own
//! async runtime, so neither entry point can live in here.
//!
//! API keys go to the OS keychain and never into the database.

use std::env;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use keyring::Entry;
use reqwest::{Client, StatusCode};
use rusqlite::{Connection, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const KEYRING_SERVICE: &str = "meterix-core";
pub const PROVIDERS: [&str; 2] = ["openrouter", "cheaperinference"];

const DATA_DIR: &str = "meterix-core";
const DB_FILE: &str = "meterix.db";
/// Overrides the database location. Used by tests; handy for second instances.
const DB_PATH_ENV: &str = "METERIX_DB";

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

/// Where a snapshot's `remaining` number came from.
///
/// Providers do not all expose a real balance, so `remaining` is only money
/// left when this says so. Anything reading `remaining` should check this
/// first, `history` included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    /// Account balance: credits bought minus credits used.
    AccountCredits,
    /// Remaining allowance under a spending cap set on the key.
    KeyCap,
    /// Not a balance. Spend so far on this credential, which only grows.
    Usage,
}

impl Basis {
    pub fn as_str(self) -> &'static str {
        match self {
            Basis::AccountCredits => "account_credits",
            Basis::KeyCap => "key_cap",
            Basis::Usage => "usage",
        }
    }

    /// Short label for a provider card or a CLI line.
    pub fn label(self) -> &'static str {
        match self {
            Basis::AccountCredits => "account balance",
            Basis::KeyCap => "key cap remaining",
            Basis::Usage => "spend so far, not a balance",
        }
    }

    /// Whether `remaining` is money left rather than money already spent.
    pub fn is_balance(self) -> bool {
        !matches!(self, Basis::Usage)
    }

    /// Unknown values read back as the least trustworthy basis rather than
    /// failing, so a future basis name cannot break `history`.
    fn from_db(value: &str) -> Basis {
        match value {
            "account_credits" => Basis::AccountCredits,
            "key_cap" => Basis::KeyCap,
            _ => Basis::Usage,
        }
    }
}

/// One provider's answer, before it is written down.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Balance {
    pub provider: &'static str,
    /// What `remaining` means. Check `basis.is_balance()` before showing
    /// `remaining` as money left.
    pub basis: Basis,
    /// The number to show a user. A real balance only when the basis says so.
    pub remaining: f64,
    /// Account balance from the provider's credits endpoint, when readable.
    pub account_credits: Option<f64>,
    /// Spend on this credential, when the provider reports it.
    pub usage: Option<f64>,
    /// How many days `usage` covers. `None` means all-time, which is how
    /// OpenRouter reports it. `Some(n)` means the last n days, which is the
    /// only thing CheaperInference offers. Never add the two together.
    pub spend_window_days: Option<u32>,
}

/// One stored row of `balance_snapshots`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub recorded_at: String,
    pub basis: Basis,
    pub account_credits: Option<f64>,
    pub usage: Option<f64>,
    pub spend_window_days: Option<u32>,
    pub remaining: f64,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// What went wrong with one provider, in a form the UI can branch on.
///
/// The v1 dashboard needs to tell an invalid key from a network failure, and
/// the v2 tray icon is colour-coded healthy/low/error, so these cases have to
/// stay distinguishable rather than collapsing into a message.
#[derive(Debug)]
pub enum ProviderError {
    /// Nothing in the keychain and nothing in the environment.
    MissingCredential(String),
    /// The provider rejected the key itself.
    Unauthorized,
    /// The key is valid but not allowed to do this, typically because it is
    /// scoped for inference only. A different problem from a bad key, and a
    /// different thing for the user to go and do about it.
    Forbidden(String),
    /// The provider is throttling balance checks.
    RateLimited,
    /// No response arrived at all.
    Unreachable(String),
    /// A response arrived but was unusable.
    BadResponse(String),
}

impl ProviderError {
    /// Stable identifier for callers that branch on the failure, such as the
    /// dashboard's error copy and the tray icon's colour. Never reword these
    /// without changing the consumers.
    pub fn kind(&self) -> &'static str {
        match self {
            ProviderError::MissingCredential(_) => "missing_credential",
            ProviderError::Unauthorized => "unauthorized",
            ProviderError::Forbidden(_) => "forbidden",
            ProviderError::RateLimited => "rate_limited",
            ProviderError::Unreachable(_) => "unreachable",
            ProviderError::BadResponse(_) => "bad_response",
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::MissingCredential(name) => write!(
                f,
                "no API key; run `set-key {name} <key>` or set the environment variable"
            ),
            ProviderError::Unauthorized => write!(f, "the API key was rejected"),
            ProviderError::Forbidden(detail) => {
                write!(f, "this key cannot read the account balance: {detail}")
            }
            ProviderError::RateLimited => write!(f, "rate limited, try again later"),
            ProviderError::Unreachable(detail) => {
                write!(f, "could not reach the provider: {detail}")
            }
            ProviderError::BadResponse(detail) => write!(f, "unexpected response: {detail}"),
        }
    }
}

impl Error for ProviderError {}

/// Turn a non-success response into an error, keeping the provider's own
/// explanation when it sent one.
///
/// "API key scope required: account:read" tells a user exactly what to fix.
/// "HTTP 403" tells them nothing. Providers that would rather not answer return
/// an HTML page for an unknown path, and that is not worth repeating back.
fn provider_error(url: &str, status: StatusCode, body: &str) -> ProviderError {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(|message| message.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            let trimmed = body.trim();
            (!trimmed.is_empty() && !trimmed.starts_with('<'))
                .then(|| trimmed.chars().take(200).collect())
        });

    match status.as_u16() {
        401 => ProviderError::Unauthorized,
        403 => ProviderError::Forbidden(
            detail.unwrap_or_else(|| "the provider refused this request".to_string()),
        ),
        429 => ProviderError::RateLimited,
        code => ProviderError::BadResponse(match detail {
            Some(detail) => format!("{url} answered HTTP {code}: {detail}"),
            None => format!("{url} answered HTTP {code}"),
        }),
    }
}

/// GET a JSON document with a bearer token, mapping anything that goes wrong
/// onto [`ProviderError`].
async fn fetch_json<T: DeserializeOwned>(
    client: &Client,
    key: &str,
    url: &str,
) -> Result<T, ProviderError> {
    let response = client
        .get(url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|error| ProviderError::Unreachable(error.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(provider_error(url, status, &body));
    }

    response
        .json::<T>()
        .await
        .map_err(|error| ProviderError::BadResponse(error.to_string()))
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn fetch_balance(&self) -> Result<Balance, ProviderError>;
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
    /// normal state for a personal key rather than a malformed response.
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

/// Pick the best available number for an OpenRouter key.
///
/// Account credits are the real balance. A per-key cap is next: it is not the
/// account balance, but it is still money the key can spend, and dropping it
/// would send a capped key backwards from cap-remaining to reporting spend.
/// Spend so far is last, and is labelled as not being a balance.
fn openrouter_balance(
    key_info: &OpenRouterData,
    account_credits: Option<f64>,
) -> Result<Balance, ProviderError> {
    let (basis, remaining) = if let Some(credits) = account_credits {
        (Basis::AccountCredits, credits)
    } else if let Some(cap) = key_info.limit_remaining {
        (Basis::KeyCap, cap)
    } else if let Some(usage) = key_info.usage {
        (Basis::Usage, usage)
    } else {
        return Err(ProviderError::BadResponse(
            "no account credits, no key cap and no usage in the response".to_string(),
        ));
    };

    Ok(Balance {
        provider: "openrouter",
        basis,
        remaining,
        account_credits,
        usage: key_info.usage,
        // OpenRouter's usage is a running total, not a window.
        spend_window_days: None,
    })
}

#[async_trait]
impl Provider for OpenRouter {
    fn name(&self) -> &'static str {
        "openrouter"
    }

    async fn fetch_balance(&self) -> Result<Balance, ProviderError> {
        let key_info =
            fetch_json::<OpenRouterResponse>(&self.client, &self.key, OPENROUTER_KEY_URL)
                .await?
                .data;

        // The real account balance. Read on every fetch even though the docs
        // say a management key is required: a personal key can read it too, and
        // the cost of guessing wrong in the other direction is losing the only
        // number this app exists to show.
        let account_credits = fetch_json::<OpenRouterCreditsResponse>(
            &self.client,
            &self.key,
            OPENROUTER_CREDITS_URL,
        )
        .await
        .ok()
        .map(|credits| credits.data.total_credits - credits.data.total_usage);

        openrouter_balance(&key_info, account_credits)
    }
}

struct CheaperInference {
    client: Client,
    key: String,
}

const CHEAPER_INFERENCE_BALANCE_URL: &str = "https://api.cheaperinference.com/v1/account/balance";
/// Spend comes from a second endpoint and is always windowed. The API rejects
/// anything wider than 90 days, so this is the widest view on offer.
const CHEAPER_INFERENCE_USAGE_URL: &str =
    "https://api.cheaperinference.com/v1/account/usage?days=90";

#[derive(Debug, Deserialize)]
struct CheaperInferenceResponse {
    /// What the account can actually spend, after anything reserved.
    available_usd: f64,
}

#[derive(Debug, Deserialize)]
struct CheaperInferenceUsage {
    /// USD billed over the window.
    billed_usd: f64,
    /// Length of that window in days, as the API reports it back.
    days: u32,
}

#[async_trait]
impl Provider for CheaperInference {
    fn name(&self) -> &'static str {
        "cheaperinference"
    }

    async fn fetch_balance(&self) -> Result<Balance, ProviderError> {
        let response = fetch_json::<CheaperInferenceResponse>(
            &self.client,
            &self.key,
            CHEAPER_INFERENCE_BALANCE_URL,
        )
        .await?;

        // Spend is a nice-to-have on top of the balance, so a failure here is
        // not worth failing the whole reading over.
        let spend = fetch_json::<CheaperInferenceUsage>(
            &self.client,
            &self.key,
            CHEAPER_INFERENCE_USAGE_URL,
        )
        .await
        .ok();

        // This endpoint reports an account balance directly, so the value is
        // both the balance and the credits figure.
        Ok(Balance {
            provider: "cheaperinference",
            basis: Basis::AccountCredits,
            remaining: response.available_usd,
            account_credits: Some(response.available_usd),
            usage: spend.as_ref().map(|usage| usage.billed_usd),
            spend_window_days: spend.map(|usage| usage.days),
        })
    }
}

/// Read a provider's key from the OS keychain, then the environment.
///
/// The keychain wins, so a stale `set-key` value shadows the environment
/// variable. Re-run `set-key` to replace it.
/// The environment variable a provider's key can be supplied through.
///
/// A keychain entry wins over this, so a value here is a fallback rather than
/// an override.
fn env_var(provider: &str) -> Option<&'static str> {
    match provider {
        "openrouter" => Some("OPENROUTER_KEY"),
        "cheaperinference" => Some("CHEAPERINFERENCE_KEY"),
        _ => None,
    }
}

/// Whether a key exists for this provider, without contacting the provider.
///
/// Used by the dashboard to decide between "not set up" and "set up but
/// failing", which are different things to show a user.
pub fn has_credential(provider: &str) -> bool {
    env_var(provider).is_some_and(|env_name| credential(provider, env_name).is_ok())
}

fn credential(provider: &str, env_name: &str) -> Result<String, ProviderError> {
    if let Ok(entry) = Entry::new(KEYRING_SERVICE, provider)
        && let Ok(key) = entry.get_password()
    {
        return Ok(key);
    }

    env::var(env_name).map_err(|_| ProviderError::MissingCredential(provider.to_string()))
}

fn build_provider(name: &str, client: &Client) -> Result<Box<dyn Provider>, ProviderError> {
    let env_name = env_var(name).ok_or_else(|| {
        ProviderError::BadResponse(format!(
            "unknown provider: {name}; use {}",
            PROVIDERS.join(", ")
        ))
    })?;

    let provider: Box<dyn Provider> = match name {
        "openrouter" => Box::new(OpenRouter {
            client: client.clone(),
            key: credential(name, env_name)?,
        }),
        "cheaperinference" => Box::new(CheaperInference {
            client: client.clone(),
            key: credential(name, env_name)?,
        }),
        _ => {
            return Err(ProviderError::BadResponse(format!(
                "unknown provider: {name}; use {}",
                PROVIDERS.join(", ")
            )));
        }
    };

    Ok(provider)
}

/// One provider's result. `Err` is per-provider, so one failure does not hide
/// the others.
pub type Outcome = (&'static str, Result<Balance, ProviderError>);

/// Fetch balances, all providers or just one.
///
/// The outer `Result` is only for a bad argument. Per-provider failures come
/// back inside the vector so callers decide what to do about them.
pub async fn fetch_balances(only: Option<&str>) -> Result<Vec<Outcome>> {
    let names: Vec<&'static str> = match only {
        Some(name) => vec![
            PROVIDERS
                .iter()
                .copied()
                .find(|provider| *provider == name)
                .ok_or_else(|| anyhow!("unknown provider: {name}; use {}", PROVIDERS.join(", ")))?,
        ],
        None => PROVIDERS.to_vec(),
    };

    // ponytail: panics on a broken TLS setup rather than returning an error;
    // switch to Client::builder().build()? if that ever matters.
    let client = Client::new();
    let mut outcomes = Vec::with_capacity(names.len());

    for name in names {
        let result = match build_provider(name, &client) {
            Ok(provider) => provider.fetch_balance().await,
            Err(error) => Err(error),
        };
        outcomes.push((name, result));
    }

    Ok(outcomes)
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Where the database lives.
///
/// Per-user application data, never the working directory: a packaged app is
/// launched with an arbitrary CWD and may be installed read-only.
pub fn database_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os(DB_PATH_ENV) {
        return Ok(PathBuf::from(path));
    }

    // ponytail: hand-rolled rather than adding the `directories` crate. Step 3
    // replaces this with Tauri's app_data_dir(), which resolves here anyway.
    let base = if cfg!(windows) {
        env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
    };

    let base = base.context("could not locate the user data directory")?;
    let directory = base.join(DATA_DIR);

    std::fs::create_dir_all(&directory)
        .with_context(|| format!("could not create {}", directory.display()))?;

    Ok(directory.join(DB_FILE))
}

/// Open the database and bring its schema up to date.
pub fn open_database() -> Result<Connection> {
    let path = database_path()?;
    let connection =
        Connection::open(&path).with_context(|| format!("could not open {}", path.display()))?;

    initialize_database(&connection)?;
    Ok(connection)
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

fn add_column_if_missing(connection: &Connection, column: &str, definition: &str) -> Result<()> {
    if column_exists(connection, "balance_snapshots", column)? {
        return Ok(());
    }

    // ponytail: column sniffing plus ALTER carries a handful of migrations.
    // Move to a PRAGMA user_version ladder once there are three or more.
    connection.execute(
        &format!("ALTER TABLE balance_snapshots ADD COLUMN {column} {definition}"),
        [],
    )?;

    Ok(())
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
            basis TEXT NOT NULL,
            account_credits REAL,
            usage REAL,
            spend_window_days INTEGER,
            recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        ",
    )?;

    add_column_if_missing(connection, "account_credits", "REAL")?;
    add_column_if_missing(connection, "usage", "REAL")?;
    add_column_if_missing(connection, "spend_window_days", "INTEGER")?;

    if !column_exists(connection, "balance_snapshots", "basis")? {
        add_column_if_missing(connection, "basis", "TEXT NOT NULL DEFAULT 'usage'")?;

        // Rows written before `basis` existed. Account credits identify
        // themselves; everything else was either spend or a cap, and the two
        // cannot be told apart after the fact, so it keeps the label that
        // claims the least.
        connection.execute(
            "UPDATE balance_snapshots SET basis = 'account_credits' \
             WHERE basis = 'usage' AND account_credits IS NOT NULL",
            [],
        )?;
    }

    for provider in PROVIDERS {
        connection.execute(
            "INSERT OR IGNORE INTO providers (name) VALUES (?1)",
            params![provider],
        )?;
    }

    Ok(())
}

pub fn save_snapshot(connection: &Connection, balance: &Balance) -> Result<()> {
    let changed = connection.execute(
        "
        INSERT INTO balance_snapshots
            (provider_id, remaining, basis, account_credits, usage, spend_window_days)
        SELECT id, ?1, ?2, ?3, ?4, ?5 FROM providers WHERE name = ?6
        ",
        params![
            balance.remaining,
            balance.basis.as_str(),
            balance.account_credits,
            balance.usage,
            balance.spend_window_days,
            balance.provider
        ],
    )?;

    // Without the check an unknown provider name writes nothing and the caller
    // still reports a saved snapshot.
    if changed == 0 {
        return Err(anyhow!("no provider row named {}", balance.provider));
    }

    Ok(())
}

pub fn history(connection: &Connection, provider: &str, limit: usize) -> Result<Vec<Snapshot>> {
    let limit = i64::try_from(limit).context("history limit is too large")?;
    let mut statement = connection.prepare(
        "
        SELECT balance_snapshots.recorded_at,
               balance_snapshots.basis,
               balance_snapshots.account_credits,
               balance_snapshots.usage,
               balance_snapshots.spend_window_days,
               balance_snapshots.remaining
        FROM balance_snapshots
        JOIN providers ON providers.id = balance_snapshots.provider_id
        WHERE providers.name = ?1
        ORDER BY balance_snapshots.recorded_at DESC, balance_snapshots.id DESC
        LIMIT ?2
        ",
    )?;

    let rows = statement.query_map(params![provider, limit], |row| {
        let basis: String = row.get(1)?;

        Ok(Snapshot {
            recorded_at: row.get(0)?,
            basis: Basis::from_db(&basis),
            account_credits: row.get(2)?,
            usage: row.get(3)?,
            spend_window_days: row.get(4)?,
            remaining: row.get(5)?,
        })
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not read balance history")
}

/// Store an API key in the OS keychain.
pub fn save_key(provider: &str, key: &str) -> Result<()> {
    if !PROVIDERS.contains(&provider) {
        return Err(anyhow!(
            "unknown provider: {provider}; use {}",
            PROVIDERS.join(", ")
        ));
    }

    let entry =
        Entry::new(KEYRING_SERVICE, provider).context("could not access the OS keychain")?;

    entry
        .set_password(key)
        .context("could not save key to the OS keychain")?;

    // A keychain that accepts a write and cannot read it back is worse than one
    // that refuses outright, because the UI looks like it worked. Confirm the
    // round trip here so the failure is reported where it happened instead of
    // surfacing later as a refresh that finds no key.
    let stored = entry
        .get_password()
        .context("the OS keychain accepted the key but could not read it back")?;

    if stored != key {
        return Err(anyhow!(
            "the OS keychain stored a different value than the one provided"
        ));
    }

    Ok(())
}

/// Delete a provider's key from the OS keychain.
///
/// Snapshots are deliberately left alone. Removing a provider should be
/// reversible, and a database of readings is not something to drop because
/// someone was tidying up their key list. Nothing reads a departed provider's
/// history, so it costs a few rows to keep it.
pub fn forget_key(provider: &str) -> Result<()> {
    if !PROVIDERS.contains(&provider) {
        return Err(anyhow!(
            "unknown provider: {provider}; use {}",
            PROVIDERS.join(", ")
        ));
    }

    let Ok(entry) = Entry::new(KEYRING_SERVICE, provider) else {
        return Ok(());
    };

    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error).context("could not remove the key from the OS keychain"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_database() -> Connection {
        let connection = Connection::open_in_memory().expect("in-memory database");
        initialize_database(&connection).expect("schema");
        connection
    }

    fn key_info(limit_remaining: Option<f64>, usage: Option<f64>) -> OpenRouterData {
        OpenRouterData {
            limit_remaining,
            usage,
        }
    }

    fn balance(basis: Basis, remaining: f64, credits: Option<f64>, usage: Option<f64>) -> Balance {
        Balance {
            provider: "openrouter",
            basis,
            remaining,
            account_credits: credits,
            usage,
            spend_window_days: None,
        }
    }

    #[test]
    fn account_credits_win_over_a_cap_and_over_usage() {
        let info = key_info(Some(50.0), Some(7.5));
        let resolved = openrouter_balance(&info, Some(20.0)).expect("a balance");

        assert_eq!(resolved.basis, Basis::AccountCredits);
        assert_eq!(resolved.remaining, 20.0);
        assert!(resolved.basis.is_balance());
        assert_eq!(resolved.usage, Some(7.5));
    }

    #[test]
    fn a_key_cap_is_used_when_credits_are_unreadable() {
        let info = key_info(Some(50.0), Some(7.5));
        let resolved = openrouter_balance(&info, None).expect("a balance");

        assert_eq!(resolved.basis, Basis::KeyCap);
        assert_eq!(resolved.remaining, 50.0);
        assert!(resolved.basis.is_balance());
        assert_eq!(resolved.account_credits, None);
    }

    #[test]
    fn usage_is_the_last_resort_and_is_not_a_balance() {
        let info = key_info(None, Some(7.5));
        let resolved = openrouter_balance(&info, None).expect("a balance");

        assert_eq!(resolved.basis, Basis::Usage);
        assert_eq!(resolved.remaining, 7.5);
        assert!(!resolved.basis.is_balance());
    }

    #[test]
    fn a_response_with_no_numbers_at_all_is_an_error() {
        let info = key_info(None, None);
        assert!(openrouter_balance(&info, None).is_err());
    }

    #[test]
    fn snapshots_come_back_with_their_basis() {
        let connection = memory_database();
        save_snapshot(&connection, &balance(Basis::Usage, 6.4, None, Some(6.4))).expect("saved");

        let rows = history(&connection, "openrouter", 10).expect("history");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].basis, Basis::Usage);
        assert!(!rows[0].basis.is_balance());
        assert_eq!(rows[0].remaining, 6.4);
        assert_eq!(rows[0].usage, Some(6.4));
        assert_eq!(rows[0].account_credits, None);
    }

    #[test]
    fn history_honours_the_limit_and_keeps_providers_apart() {
        let connection = memory_database();

        for value in 1..=5 {
            save_snapshot(
                &connection,
                &balance(
                    Basis::AccountCredits,
                    f64::from(value),
                    Some(f64::from(value)),
                    None,
                ),
            )
            .expect("saved");
        }
        save_snapshot(
            &connection,
            &Balance {
                provider: "cheaperinference",
                basis: Basis::AccountCredits,
                remaining: 99.0,
                account_credits: Some(99.0),
                usage: None,
                spend_window_days: None,
            },
        )
        .expect("saved");

        assert_eq!(
            history(&connection, "openrouter", 3)
                .expect("history")
                .len(),
            3
        );
        assert_eq!(
            history(&connection, "cheaperinference", 10)
                .expect("history")
                .len(),
            1
        );
    }

    #[test]
    fn unknown_provider_writes_nothing_and_says_so() {
        let connection = memory_database();
        let mut orphan = balance(Basis::Usage, 1.0, None, Some(1.0));
        orphan.provider = "nowhere";

        assert!(save_snapshot(&connection, &orphan).is_err());
        assert!(
            history(&connection, "openrouter", 10)
                .expect("history")
                .is_empty()
        );
    }

    #[test]
    fn rows_from_before_the_basis_column_still_open() {
        let connection = Connection::open_in_memory().expect("in-memory database");

        // The schema as it was before `basis` existed.
        connection
            .execute_batch(
                "
                CREATE TABLE providers (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL UNIQUE
                );
                CREATE TABLE balance_snapshots (
                    id INTEGER PRIMARY KEY,
                    provider_id INTEGER NOT NULL REFERENCES providers(id),
                    remaining REAL NOT NULL,
                    account_credits REAL,
                    usage REAL,
                    recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );
                INSERT INTO providers (id, name) VALUES (1, 'openrouter');
                INSERT INTO balance_snapshots (provider_id, remaining, account_credits, usage)
                    VALUES (1, 6.4, NULL, NULL);
                INSERT INTO balance_snapshots (provider_id, remaining, account_credits, usage)
                    VALUES (1, 12.0, 12.0, NULL);
                ",
            )
            .expect("legacy schema");

        initialize_database(&connection).expect("migration");
        // Opening twice must not try to add the column again.
        initialize_database(&connection).expect("migration is idempotent");

        let rows = history(&connection, "openrouter", 10).expect("history");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].basis, Basis::AccountCredits);
        assert_eq!(rows[0].remaining, 12.0);
        // No credits recorded, so nothing could classify it as a balance.
        assert_eq!(rows[1].basis, Basis::Usage);
        assert!(!rows[1].basis.is_balance());
    }

    #[test]
    fn provider_errors_keep_the_providers_own_wording() {
        let body = r#"{"error":{"message":"API key scope required: account:read.","code":"insufficient_scope"}}"#;

        let error = provider_error("https://x/v1/account/balance", StatusCode::FORBIDDEN, body);
        assert_eq!(error.kind(), "forbidden");
        assert!(error.to_string().contains("account:read"));

        // A valid key missing a scope is not the same failure as a bad key.
        assert_eq!(
            provider_error("u", StatusCode::UNAUTHORIZED, "").kind(),
            "unauthorized"
        );
        assert_eq!(
            provider_error("u", StatusCode::TOO_MANY_REQUESTS, "").kind(),
            "rate_limited"
        );

        // An HTML 404 page is not worth repeating back to anyone.
        let error = provider_error(
            "https://x/v1/nope",
            StatusCode::NOT_FOUND,
            "<!DOCTYPE html><html><body>not found</body></html>",
        );
        assert!(!error.to_string().contains("DOCTYPE"));
        assert!(error.to_string().contains("answered HTTP 404"));
    }

    /// A windowed spend figure and an all-time one must stay tellable apart,
    /// or the two get added together and the total means nothing.
    #[test]
    fn a_windowed_spend_keeps_its_window() {
        let connection = memory_database();

        let mut windowed = balance(Basis::AccountCredits, 13.51, Some(13.51), Some(1.49));
        windowed.provider = "cheaperinference";
        windowed.spend_window_days = Some(90);
        save_snapshot(&connection, &windowed).expect("saved");

        let rows = history(&connection, "cheaperinference", 1).expect("history");
        assert_eq!(rows[0].usage, Some(1.49));
        assert_eq!(rows[0].spend_window_days, Some(90));

        save_snapshot(
            &connection,
            &balance(Basis::AccountCredits, 6.4, Some(6.4), Some(3.6)),
        )
        .expect("saved");

        let rows = history(&connection, "openrouter", 1).expect("history");
        assert_eq!(rows[0].usage, Some(3.6));
        assert_eq!(rows[0].spend_window_days, None);
    }

    #[test]
    fn unknown_basis_text_reads_back_as_usage() {
        assert_eq!(Basis::from_db("something_new"), Basis::Usage);
        assert_eq!(Basis::from_db("key_cap"), Basis::KeyCap);
    }

    /// The value stored in the `basis` column and the value the UI receives
    /// over IPC have to agree, because the chart filters on one and the labels
    /// switch on the other.
    #[test]
    fn basis_serializes_exactly_as_it_is_stored() {
        for basis in [Basis::AccountCredits, Basis::KeyCap, Basis::Usage] {
            assert_eq!(
                serde_json::to_value(basis).expect("serializes"),
                serde_json::json!(basis.as_str())
            );
        }
    }
}
