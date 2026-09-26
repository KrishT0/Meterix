//! Core of Meterix: read AI provider credit balances and keep them in SQLite.
//!
//! There is no `main` here on purpose. The CLI (`src/main.rs`) and the future
//! Tauri app both link this crate, and Tauri owns its own argv and its own
//! async runtime, so neither entry point can live in here.
//!
//! API keys go to the OS keychain and never into the database.

use std::collections::{HashMap, HashSet};
use std::env;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use keyring::Entry;
use reqwest::{Client, StatusCode};
use rusqlite::OptionalExtension;
use rusqlite::params;
// Re-exported so the Tauri shell can name a connection without depending on
// rusqlite itself, which would risk the two crates resolving different versions.
pub use rusqlite::Connection;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const KEYRING_SERVICE: &str = "meterix-core";

/// The provider kinds this build can talk to: the *adapters*, not the set being
/// tracked.
///
/// Each entry names a hand-written adapter, because reading a provider's balance
/// is code — OpenRouter's is a three-way choice between account credits, a key cap
/// and spend, which no configuration describes. What is data is which of these the
/// app actually tracks, which lives in the `providers` table. Adding a kind is a
/// code change; turning one on or off is not.
///
/// **The order matters.** `provider_for_key` takes the first entry whose prefix
/// matches, and an OpenRouter key also starts with DeepSeek's `sk-`, so the looser
/// prefix must not be checked first.
pub const PROVIDERS: [&str; 4] = ["openrouter", "cheaperinference", "deepseek", "elevenlabs"];

/// The folder the app uses unless it has been moved: `~/.meterix`.
///
/// A dot directory in the home folder rather than a platform data directory, the
/// way tools like this usually live, and one path to remember instead of three.
const DATA_DIR: &str = ".meterix";
/// The same folder under its old name, in the platform data directory. Only used
/// to find a database written by an earlier build so it can be carried over.
const LEGACY_DATA_DIR: &str = "meterix-core";
const DB_FILE: &str = "meterix.db";
/// Inside the anchor, a one-line file naming a different folder for the database.
const LOCATION_FILE: &str = "location";
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
    /// A limited allowance that is not money at all: ElevenLabs counts
    /// characters, not dollars. It sits apart from the other three because a
    /// character cannot be added to a dollar total or drawn on a dollar axis, and
    /// a dollar threshold says nothing about it.
    Quota,
}

impl Basis {
    pub fn as_str(self) -> &'static str {
        match self {
            Basis::AccountCredits => "account_credits",
            Basis::KeyCap => "key_cap",
            Basis::Usage => "usage",
            Basis::Quota => "quota",
        }
    }

    /// Short label for a provider card or a CLI line.
    pub fn label(self) -> &'static str {
        match self {
            Basis::AccountCredits => "account balance",
            Basis::KeyCap => "key cap remaining",
            Basis::Usage => "spend so far, not a balance",
            Basis::Quota => "allowance left, not money",
        }
    }

    /// Whether `remaining` is money left rather than something else.
    ///
    /// A quota is not money, so it is excluded here for the same reason spend is:
    /// this is the flag that decides what can be compared to a dollar threshold,
    /// summed into a dollar total, or drawn on the dollar chart.
    pub fn is_balance(self) -> bool {
        matches!(self, Basis::AccountCredits | Basis::KeyCap)
    }

    /// Unknown values read back as the least trustworthy basis rather than
    /// failing, so a future basis name cannot break `history`.
    fn from_db(value: &str) -> Basis {
        match value {
            "account_credits" => Basis::AccountCredits,
            "key_cap" => Basis::KeyCap,
            "quota" => Basis::Quota,
            _ => Basis::Usage,
        }
    }
}

/// One provider's answer, before it is written down.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Balance {
    /// The provider's id as the database knows it. A `String` rather than a
    /// compile-time string because a provider can now be a row somebody added,
    /// and an id nobody agreed on in advance cannot be `&'static`.
    pub provider: String,
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
    /// The provider's own idea of "low", when it publishes one. Recorded
    /// alongside the reading for the same reason `basis` is: a number is only
    /// usable next to what it means, and the tray and the notifier resolve a
    /// threshold from stored state rather than by re-fetching.
    pub provider_threshold: Option<f64>,
    /// Which credential produced this. See [`key_fingerprint`].
    pub key_fingerprint: Option<String>,
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
    /// The provider's own low threshold as of this reading, or `None` when it
    /// published none.
    pub provider_threshold: Option<f64>,
    /// Which credential produced this reading, or `None` for rows written
    /// before fingerprints existed.
    pub key_fingerprint: Option<String>,
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

/// The identifier `kind()` returns when no key is stored at all.
///
/// Named rather than written out at each call site, because callers that only
/// hold the recorded string — the tray colour, for one — have to recognise it
/// without repeating the literal, and a rename would otherwise go unnoticed.
pub const MISSING_CREDENTIAL_KIND: &str = "missing_credential";

impl ProviderError {
    /// Stable identifier for callers that branch on the failure, such as the
    /// dashboard's error copy and the tray icon's colour. Never reword these
    /// without changing the consumers.
    pub fn kind(&self) -> &'static str {
        match self {
            ProviderError::MissingCredential(_) => MISSING_CREDENTIAL_KIND,
            ProviderError::Unauthorized => "unauthorized",
            ProviderError::Forbidden(_) => "forbidden",
            ProviderError::RateLimited => "rate_limited",
            ProviderError::Unreachable(_) => "unreachable",
            ProviderError::BadResponse(_) => "bad_response",
        }
    }

    /// Whether this failure means the credential cannot be used, as opposed to
    /// a problem that says nothing about the key at all.
    ///
    /// Only these are worth interrupting someone over. Telling a user their key
    /// has stopped working when their wifi dropped is worse than saying nothing:
    /// it sends them to rotate a key that was fine. A missing credential is
    /// excluded too, because that is what a fresh install looks like and the
    /// dashboard is already asking for one.
    pub fn credential_is_broken(&self) -> bool {
        matches!(
            self,
            ProviderError::Unauthorized | ProviderError::Forbidden(_)
        )
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
    fetch_json_as(client, key, None, url).await
}

/// `header` names a provider-specific header to carry the key instead of
/// `Authorization: Bearer`. ElevenLabs wants `xi-api-key`, and sending a bearer
/// header there fails in a way that reads as a rejected key rather than as the
/// wrong header.
async fn fetch_json_as<T: DeserializeOwned>(
    client: &Client,
    key: &str,
    header: Option<&str>,
    url: &str,
) -> Result<T, ProviderError> {
    let request = match header {
        Some(name) => client.get(url).header(name, key),
        None => client.get(url).bearer_auth(key),
    };

    let response = request
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
    async fn fetch_balance(&self) -> Result<Balance, ProviderError>;
}

/// A gateway that speaks OpenRouter's own shape: a key endpoint and a credits
/// endpoint under `/api/v1`.
///
/// Its `base_url` is a field rather than a constant because the shape is not
/// OpenRouter's alone. Anything that cloned those two endpoints can be read by
/// this one adapter, so the compiled `openrouter` kind and a gateway row that
/// names `openrouter_compatible` run exactly the same code rather than two
/// copies of it that would drift.
struct OpenRouterCompatible {
    client: Client,
    key: String,
    /// The row's id, which is what a reading is filed under.
    provider: String,
    /// Where the gateway lives, with no trailing slash.
    base_url: String,
}

const OPENROUTER_BASE_URL: &str = "https://openrouter.ai";

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
    provider: &str,
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
        provider: provider.to_string(),
        basis,
        remaining,
        account_credits,
        usage: key_info.usage,
        // OpenRouter's usage is a running total, not a window.
        spend_window_days: None,
        // OpenRouter publishes no threshold of its own.
        provider_threshold: None,
        // Filled in by the caller, which is where the key is known.
        key_fingerprint: None,
    })
}

#[async_trait]
impl Provider for OpenRouterCompatible {
    async fn fetch_balance(&self) -> Result<Balance, ProviderError> {
        let key_url = format!("{}/api/v1/key", self.base_url);
        let credits_url = format!("{}/api/v1/credits", self.base_url);

        let key_info = fetch_json::<OpenRouterResponse>(&self.client, &self.key, &key_url)
            .await?
            .data;

        // The real account balance. Read on every fetch even though the docs
        // say a management key is required: a personal key can read it too, and
        // the cost of guessing wrong in the other direction is losing the only
        // number this app exists to show.
        let account_credits = fetch_json::<OpenRouterCreditsResponse>(
            &self.client,
            &self.key,
            &credits_url,
        )
        .await
        .ok()
        .map(|credits| credits.data.total_credits - credits.data.total_usage);

        openrouter_balance(&self.provider, &key_info, account_credits)
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
    /// The balance this account auto-recharges at. This is the provider's own
    /// answer to "when am I running low", so it is a better default than a flat
    /// dollar figure that knows nothing about the account. Advisory, so a
    /// missing or null field means "no opinion" rather than a failed reading.
    #[serde(default)]
    threshold_usd: Option<f64>,
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
            provider: "cheaperinference".to_string(),
            basis: Basis::AccountCredits,
            remaining: response.available_usd,
            account_credits: Some(response.available_usd),
            usage: spend.as_ref().map(|usage| usage.billed_usd),
            spend_window_days: spend.map(|usage| usage.days),
            provider_threshold: response.threshold_usd,
            // Filled in by the caller, which is where the key is known.
            key_fingerprint: None,
        })
    }
}

/// DeepSeek publishes an actual prepaid balance, in the currency the account was
/// funded in.
struct DeepSeek {
    client: Client,
    key: String,
}

const DEEPSEEK_BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

#[derive(Debug, Deserialize)]
struct DeepSeekResponse {
    #[serde(default)]
    balance_infos: Vec<DeepSeekBalance>,
}

#[derive(Debug, Deserialize)]
struct DeepSeekBalance {
    /// `USD` or `CNY`.
    currency: String,
    /// A string in the response despite reading as a number, so it is parsed
    /// rather than deserialised as a float.
    total_balance: String,
}

#[async_trait]
impl Provider for DeepSeek {

    async fn fetch_balance(&self) -> Result<Balance, ProviderError> {
        let response =
            fetch_json::<DeepSeekResponse>(&self.client, &self.key, DEEPSEEK_BALANCE_URL).await?;

        // An account can be funded in more than one currency and every number the
        // app shows is compared against a dollar threshold, so only the USD entry
        // can be used. A CNY-only account is refused rather than relabelled: the
        // alternative is a figure in yuan sitting under a dollar sign, which is
        // worse than an error because it looks like an answer.
        let remaining = deepseek_usd_balance(&response)?;

        Ok(Balance {
            provider: "deepseek".to_string(),
            basis: Basis::AccountCredits,
            remaining,
            account_credits: Some(remaining),
            // DeepSeek reports a balance and nothing else.
            usage: None,
            spend_window_days: None,
            provider_threshold: None,
            // Filled in by the caller, which is where the key is known.
            key_fingerprint: None,
        })
    }
}

/// The USD figure out of a DeepSeek balance response.
///
/// Split out from the request so the currency choice can be tested directly: it is
/// the one judgement in the adapter, and getting it wrong puts a figure in yuan
/// under a dollar sign, where it looks like an answer.
fn deepseek_usd_balance(response: &DeepSeekResponse) -> Result<f64, ProviderError> {
    let usd = response
        .balance_infos
        .iter()
        .find(|info| info.currency.eq_ignore_ascii_case("USD"))
        .ok_or_else(|| {
            let found = response
                .balance_infos
                .first()
                .map_or("no currency at all", |info| info.currency.as_str());

            ProviderError::BadResponse(format!(
                "this account reports a balance in {found}, and only USD can be compared against \
                 a dollar threshold"
            ))
        })?;

    usd.total_balance
        .trim()
        .parse::<f64>()
        .map_err(|_| ProviderError::BadResponse(format!("unreadable balance: {}", usd.total_balance)))
}

/// ElevenLabs sells a monthly character allowance, so what it has to report is a
/// quota rather than money.
struct ElevenLabs {
    client: Client,
    key: String,
}

const ELEVENLABS_SUBSCRIPTION_URL: &str = "https://api.elevenlabs.io/v1/user/subscription";

#[derive(Debug, Deserialize)]
struct ElevenLabsSubscription {
    /// Characters used in the current period.
    character_count: f64,
    /// Characters allowed in the current period.
    character_limit: f64,
}

#[async_trait]
impl Provider for ElevenLabs {

    async fn fetch_balance(&self) -> Result<Balance, ProviderError> {
        // `xi-api-key`, not a bearer token.
        let subscription = fetch_json_as::<ElevenLabsSubscription>(
            &self.client,
            &self.key,
            Some("xi-api-key"),
            ELEVENLABS_SUBSCRIPTION_URL,
        )
        .await?;

        // Deliberately not clamped at zero. Overage is allowed up to an extension
        // a workspace admin sets, and a negative figure is the honest way to say
        // "past the allowance" rather than "exactly out".
        let remaining = subscription.character_limit - subscription.character_count;

        Ok(Balance {
            provider: "elevenlabs".to_string(),
            // The basis is what keeps a character count out of the dollar total and
            // off the money chart.
            basis: Basis::Quota,
            remaining,
            // Nothing here is money, so these stay empty rather than holding a
            // character count in a column the rest of the app reads as dollars.
            account_credits: None,
            usage: None,
            spend_window_days: None,
            provider_threshold: None,
            key_fingerprint: None,
        })
    }
}

/// Static facts about a supported provider, kept in one place so the pieces
/// cannot drift apart.
struct ProviderSpec {
    /// How the provider is written for a person. Not the id: capitalising
    /// "cheaperinference" with CSS loses the internal capital.
    display_name: &'static str,
    /// Environment variable its key can be supplied through.
    env_var: &'static str,
    /// What a key for this provider starts with. Only ever used to notice a key
    /// filed under the wrong provider, so a provider changing its format
    /// degrades the hint rather than breaking anything.
    key_prefix: &'static str,
}

fn spec(provider: &str) -> Option<ProviderSpec> {
    match provider {
        "openrouter" => Some(ProviderSpec {
            display_name: "OpenRouter",
            env_var: "OPENROUTER_KEY",
            key_prefix: "sk-or-",
        }),
        "cheaperinference" => Some(ProviderSpec {
            display_name: "CheaperInference",
            env_var: "CHEAPERINFERENCE_KEY",
            key_prefix: "ci_",
        }),
        "deepseek" => Some(ProviderSpec {
            display_name: "DeepSeek",
            env_var: "DEEPSEEK_KEY",
            key_prefix: "sk-",
        }),
        "elevenlabs" => Some(ProviderSpec {
            display_name: "ElevenLabs",
            env_var: "ELEVENLABS_KEY",
            // An underscore, unlike every other prefix here.
            key_prefix: "sk_",
        }),
        _ => None,
    }
}

/// The environment variable a provider's key can be supplied through.
///
/// A keychain entry wins over this, so a value here is a fallback rather than
/// an override.
fn env_var(provider: &str) -> Option<&'static str> {
    spec(provider).map(|spec| spec.env_var)
}

/// The provider's name as it should appear to a person.
///
/// An unrecognised provider is returned unchanged rather than blanked, so a
/// missing entry shows up as itself instead of as nothing.
pub fn display_name(provider: &str) -> &str {
    spec(provider).map_or(provider, |spec| spec.display_name)
}

/// A short, safe way to say *which* key is stored: its format prefix and its
/// last four characters.
///
/// Enough to tell two keys apart, and to notice one filed under the wrong
/// provider, without putting the secret on screen.
/// A stable, non-reversible label for an API key.
///
/// Snapshots record which credential produced them, so replacing a key with a
/// different account does not splice two accounts into one trend line. The key
/// itself must never reach the database, so only the first 8 bytes of its
/// SHA-256 are kept: enough to tell two keys apart, useless for recovering
/// either one.
///
/// Not a security boundary. It is a label that lets history be attributed.
pub fn key_fingerprint(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let mut out = String::with_capacity(16);

    for byte in &digest[..8] {
        out.push_str(&format!("{byte:02x}"));
    }

    out
}

pub fn credential_hint(provider: &str) -> Option<String> {
    let key = credential(provider, env_var(provider)?).ok()?;
    Some(mask_key(&key))
}

/// The fingerprint of the credential a provider is configured with right now.
///
/// `None` when no key is stored, which is not the same as an empty fingerprint:
/// nothing is configured, so no reading can belong to it.
pub fn provider_fingerprint(provider: &str) -> Option<String> {
    let key = credential(provider, env_var(provider)?).ok()?;
    Some(key_fingerprint(&key))
}

fn mask_key(key: &str) -> String {
    let characters: Vec<char> = key.chars().collect();

    if characters.len() <= 12 {
        return "\u{2022}".repeat(8);
    }

    let head: String = characters[..8].iter().collect();
    let tail: String = characters[characters.len() - 4..].iter().collect();
    format!("{head}\u{2026}{tail}")
}

/// Which provider a key's format belongs to, if any.
///
/// Advisory only. Prefixes are the providers' conventions rather than a
/// contract, so this is never allowed to decide anything on its own.
pub fn provider_for_key(key: &str) -> Option<&'static str> {
    PROVIDERS
        .iter()
        .copied()
        .find(|provider| spec(provider).is_some_and(|spec| key.starts_with(spec.key_prefix)))
}

/// When a key is refused by one provider but its format belongs to another,
/// say so.
///
/// A key filed under the wrong provider comes back as rejected, which sends
/// someone off to check a key that was perfectly good.
pub fn misdirected_key_hint(selected: &str, key: &str) -> Option<String> {
    let other = provider_for_key(key)?;
    if other == selected {
        return None;
    }

    Some(format!(
        "It starts with `{}`, the {} format, so check which provider is selected.",
        spec(other)?.key_prefix,
        display_name(other)
    ))
}

fn unknown_provider(name: &str) -> ProviderError {
    ProviderError::BadResponse(format!(
        "unknown provider: {name}; use {}",
        PROVIDERS.join(", ")
    ))
}

/// Build a provider that reads its key from the given source, rather than from
/// whatever is stored.
fn provider_with_key(name: &str, client: &Client, key: String) -> Option<Box<dyn Provider>> {
    match name {
        "openrouter" => Some(Box::new(OpenRouterCompatible {
            client: client.clone(),
            key,
            provider: "openrouter".to_string(),
            base_url: OPENROUTER_BASE_URL.to_string(),
        })),
        "cheaperinference" => Some(Box::new(CheaperInference {
            client: client.clone(),
            key,
        })),
        "deepseek" => Some(Box::new(DeepSeek {
            client: client.clone(),
            key,
        })),
        "elevenlabs" => Some(Box::new(ElevenLabs {
            client: client.clone(),
            key,
        })),
        _ => None,
    }
}

/// Read a candidate key's balance without storing it.
///
/// Called before replacing a stored key, so a typo cannot destroy a working
/// one. Nothing here touches the keychain.
pub async fn verify_key(provider: &str, key: &str) -> Result<Balance, ProviderError> {
    // ponytail: panics on a broken TLS setup, matching fetch_selected.
    let client = Client::new();
    let candidate = provider_with_key(provider, &client, key.to_string())
        .ok_or_else(|| unknown_provider(provider))?;

    candidate.fetch_balance().await
}

/// What came of offering a key for saving.
pub enum SaveOutcome {
    /// The key works and a balance was read. Stored.
    Verified(Balance),
    /// The key was stored, but no balance could be read from it.
    SavedUnverified(String),
    /// The provider refused the key. **Nothing was written.**
    Rejected(String),
}

impl SaveOutcome {
    /// Stable identifier for callers that render the result.
    pub fn status(&self) -> &'static str {
        match self {
            SaveOutcome::Verified(_) => "saved_verified",
            SaveOutcome::SavedUnverified(_) => "saved_unverified",
            SaveOutcome::Rejected(_) => "rejected",
        }
    }

    pub fn balance(&self) -> Option<f64> {
        match self {
            SaveOutcome::Verified(balance) => Some(balance.remaining),
            _ => None,
        }
    }

    pub fn message(&self) -> Option<&str> {
        match self {
            SaveOutcome::Verified(_) => None,
            SaveOutcome::SavedUnverified(message) | SaveOutcome::Rejected(message) => Some(message),
        }
    }
}

/// Turn a lowercase fragment into a sentence, so fragments can be joined
/// without producing "rejected It starts with".
///
/// Error `Display` strings are written to follow a colon, so they are lowercase
/// and unpunctuated. Stitching those together needs this.
fn as_sentence(fragment: &str) -> String {
    let mut characters = fragment.chars();
    let Some(first) = characters.next() else {
        return String::new();
    };

    let mut sentence: String = first.to_uppercase().collect();
    sentence.push_str(characters.as_str());

    if !sentence.ends_with(['.', '!', '?']) {
        sentence.push('.');
    }

    sentence
}

/// Replace a provider's stored key, but only after checking the candidate.
///
/// The existing keychain entry is left untouched until the candidate has proved
/// itself, so a typo cannot destroy a working key. A key the provider actively
/// refuses is the one case that writes nothing.
pub async fn save_verified_key(provider: &str, key: &str) -> Result<SaveOutcome> {
    // A provider that is merely unreachable has not said the key is wrong, so
    // refusing to save would leave someone offline unable to set a key at all.
    match verify_key(provider, key).await {
        Ok(balance) => {
            save_key(provider, key)?;
            Ok(SaveOutcome::Verified(balance))
        }

        Err(error) if error.kind() != "unauthorized" => {
            save_key(provider, key)?;
            Ok(SaveOutcome::SavedUnverified(error.to_string()))
        }

        Err(error) => {
            let mut message = as_sentence(&error.to_string());
            if let Some(hint) = misdirected_key_hint(provider, key) {
                message.push(' ');
                message.push_str(&hint);
            }

            Ok(SaveOutcome::Rejected(message))
        }
    }
}

/// Read a provider's key from the OS keychain, then the environment.
///
/// The keychain wins, so a stale `set-key` value shadows the environment
/// variable. Re-run `set-key` to replace it.
fn credential(provider: &str, env_name: &str) -> Result<String, ProviderError> {
    if let Ok(entry) = Entry::new(KEYRING_SERVICE, provider)
        && let Ok(key) = entry.get_password()
    {
        return Ok(key);
    }

    env::var(env_name).map_err(|_| ProviderError::MissingCredential(provider.to_string()))
}

/// The environment variable still supplying a key for this provider, if one is.
///
/// The environment is read on its own, so this only means anything once the
/// keychain entry is gone: it answers "would a fetch find a key now".
fn env_credential(provider: &str) -> Option<&'static str> {
    let name = env_var(provider)?;
    env::var(name).ok().map(|_| name)
}

fn build_provider(
    name: &str,
    client: &Client,
) -> Result<(Box<dyn Provider>, String), ProviderError> {
    let env_name = env_var(name).ok_or_else(|| unknown_provider(name))?;
    let key = credential(name, env_name)?;
    // Fingerprinted here, where the key is in hand and before it is dropped.
    let fingerprint = key_fingerprint(&key);

    provider_with_key(name, client, key)
        .map(|provider| (provider, fingerprint))
        .ok_or_else(|| unknown_provider(name))
}

/// One provider's result. `Err` is per-provider, so one failure does not hide
/// the others.
pub type Outcome = (String, Result<Balance, ProviderError>);

/// Fetch balances, all providers or just one.
///
/// The outer `Result` is only for a bad argument. Per-provider failures come
/// back inside the vector so callers decide what to do about them.
/// The providers a request covers: the one named, or every provider.
///
/// An unknown name is an error rather than an empty fetch, so a typo cannot look
/// like "there was nothing to do".
pub fn requested_providers(only: Option<&str>) -> Result<Vec<String>> {
    match only {
        Some(name) => Ok(vec![
            PROVIDERS
                .iter()
                .copied()
                .find(|provider| *provider == name)
                .ok_or_else(|| anyhow!("unknown provider: {name}; use {}", PROVIDERS.join(", ")))?
                .to_string(),
        ]),
        None => Ok(PROVIDERS
            .iter()
            .map(|name| (*name).to_string())
            .collect()),
    }
}

/// Fetch an explicit set of providers, in the order given.
///
/// Every caller ends up here — the command line, the window's Refresh and the
/// poller — so the ordering and the fingerprint handling cannot drift apart. What
/// differs is only how each one works out its set: named explicitly, everything
/// tracked, or whatever is due.
pub async fn fetch_selected(names: &[String]) -> Result<Vec<Outcome>> {
    // ponytail: panics on a broken TLS setup rather than returning an error;
    // switch to Client::builder().build()? if that ever matters.
    let client = Client::new();
    let mut outcomes = Vec::with_capacity(names.len());

    for name in names {
        let result = match build_provider(name, &client) {
            Ok((provider, fingerprint)) => match provider.fetch_balance().await {
                Ok(mut balance) => {
                    balance.key_fingerprint = Some(fingerprint);
                    Ok(balance)
                }
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        outcomes.push((name.clone(), result));
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
///
/// `~/.meterix/meterix.db` unless the user has moved it, which is recorded in the
/// anchor rather than in the database — the location has to be known before there
/// is a database to ask.
pub fn database_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os(DB_PATH_ENV) {
        return Ok(PathBuf::from(path));
    }

    let directory = data_directory()?;

    std::fs::create_dir_all(&directory)
        .with_context(|| format!("could not create {}", directory.display()))?;

    Ok(directory.join(DB_FILE))
}

/// The folder the database is actually read from.
///
/// The anchor is fixed at `~/.meterix`; a `location` file inside it names somewhere
/// else. That indirection is why a moved database works at all: the choice cannot
/// live in the database it is about.
pub fn data_directory() -> Result<PathBuf> {
    data_directory_in(&anchor_directory())
}

fn data_directory_in(anchor: &Path) -> Result<PathBuf> {
    let Ok(recorded) = std::fs::read_to_string(anchor.join(LOCATION_FILE)) else {
        return Ok(anchor.to_path_buf());
    };

    let recorded = recorded.trim();
    if recorded.is_empty() {
        return Ok(anchor.to_path_buf());
    }

    let moved = PathBuf::from(recorded);
    Ok(if moved.is_absolute() {
        moved
    } else {
        anchor.join(moved)
    })
}

/// Point the app at a different folder for the database.
///
/// Writes the `location` file; it takes effect the next time the database is
/// opened, which is the next launch. Copying the database itself is the caller's
/// job, because only the caller can tell the user what happened if it fails.
pub fn set_data_directory(directory: &Path) -> Result<()> {
    let anchor = anchor_directory();
    std::fs::create_dir_all(&anchor)
        .with_context(|| format!("could not create {}", anchor.display()))?;

    let location = anchor.join(LOCATION_FILE);
    std::fs::write(&location, directory.display().to_string())
        .with_context(|| format!("could not write {}", location.display()))
}

/// Carry a database written by an earlier build into the new default location.
///
/// Called once at startup, before anything opens the database. The old location
/// was a platform data directory (`%APPDATA%\meterix-core` and its equivalents);
/// the new one is `~/.meterix`, so without this every existing install would open
/// a brand new database and look empty.
///
/// Copies rather than moves. The file it leaves behind is the only copy of
/// readings that cannot be fetched again, and keeping it costs a few kilobytes.
pub fn adopt_legacy_database() -> Result<Option<PathBuf>> {
    // The override names a database explicitly, and a `location` file means the
    // user has already said where they want it. Neither is ours to second-guess.
    if env::var_os(DB_PATH_ENV).is_some() {
        return Ok(None);
    }

    let anchor = anchor_directory();
    if data_directory_in(&anchor)? != anchor {
        return Ok(None);
    }

    let Some(legacy) = legacy_database_path() else {
        return Ok(None);
    };
    let Some(legacy) = legacy.parent() else {
        return Ok(None);
    };

    adopt_from(legacy, &anchor)
}

/// The copy itself, taking both folders so it can be exercised without touching
/// a real home directory.
fn adopt_from(legacy: &Path, target: &Path) -> Result<Option<PathBuf>> {
    let source = legacy.join(DB_FILE);
    let destination = target.join(DB_FILE);

    // A database already at the target is never overwritten, and an absent one at
    // the old location is not an error — most installs will never have had one.
    if destination.exists() || !source.exists() {
        return Ok(None);
    }

    std::fs::create_dir_all(target)
        .with_context(|| format!("could not create {}", target.display()))?;
    std::fs::copy(&source, &destination)
        .with_context(|| format!("could not carry {} over", source.display()))?;

    // Without a checkpointed write-ahead log the copy can be missing the newest
    // readings, or miss the schema entirely.
    let sidecar = PathBuf::from(format!("{}-wal", source.display()));
    if sidecar.exists() {
        let _ = std::fs::copy(&sidecar, format!("{}-wal", destination.display()));
    }

    Ok(Some(source))
}

/// The user's home folder.
///
/// `USERPROFILE` first on Windows: it is the one that means "this user's home",
/// where `HOME` is set by whichever shell happened to launch the app and can point
/// somewhere else entirely. Both are tried, so a stripped environment still works.
fn home() -> Option<PathBuf> {
    if cfg!(windows) && env::var_os("USERPROFILE").is_some() {
        return env::var_os("USERPROFILE").map(PathBuf::from);
    }

    env::var_os("HOME").map(PathBuf::from)
}

/// The fixed folder everything hangs off: `~/.meterix`.
fn anchor_directory() -> PathBuf {
    home().map_or_else(|| PathBuf::from(DATA_DIR), |home| anchor_directory_in(&home))
}

/// Split out from the above so a test can hand it a folder instead of depending on
/// whoever is running it having a home directory and no database.
fn anchor_directory_in(home: &Path) -> PathBuf {
    home.join(DATA_DIR)
}

/// Where an earlier build kept the database: a platform data directory.
///
/// Only used to find one so it can be carried over. The `directories` crate would
/// cover more platforms for less code, but this has to keep resolving the *old*
/// layout, which is frozen, so a handful of lines is the honest size of it.
fn legacy_database_path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        home().map(|home| home.join("Library/Application Support"))
    } else {
        env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home().map(|home| home.join(".local/share")))
    }?;

    Some(base.join(LEGACY_DATA_DIR).join(DB_FILE))
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

/// Where an export should be written: the chosen folder, or the data directory.
///
/// Resolved here rather than at each call site, so the CLI and the window cannot
/// disagree about where the file went.
pub fn export_directory(settings: &Settings) -> Result<PathBuf> {
    match settings.export_directory.as_deref() {
        Some(chosen) => Ok(PathBuf::from(chosen)),
        None => data_directory(),
    }
}

/// Move the database to a different folder, keeping the readings.
///
/// Records the choice and copies the file; the original is left where it was, so a
/// mistake is recoverable by deleting the `location` file rather than by finding a
/// backup. Takes effect on the next open, because moving the file out from under a
/// live connection is how a database ends up half in one place and half in another.
///
/// Refuses to overwrite a database that is already there. Pointing at a folder
/// with someone else's readings in it should fail loudly, not replace them.
pub fn relocate_data_directory(directory: &Path) -> Result<PathBuf> {
    let current = database_path()?;
    let target = directory.join(DB_FILE);

    std::fs::create_dir_all(directory)
        .with_context(|| format!("could not create {}", directory.display()))?;

    if current != target {
        if target.exists() {
            return Err(anyhow!(
                "{} already has a database in it; pick an empty folder",
                directory.display()
            ));
        }

        if current.exists() {
            std::fs::copy(&current, &target)
                .with_context(|| format!("could not copy the database to {}", target.display()))?;
        }
    }

    set_data_directory(directory)?;

    Ok(target)
}

/// The schema this build writes, recorded in `PRAGMA user_version`.
///
/// Raise this and add a step to `migrate` whenever a column changes. A step added
/// without raising the version never runs, which is the one failure this
/// arrangement can produce — `every_step_of_the_ladder_is_reachable` is there to
/// catch it.
const SCHEMA_VERSION: i32 = 6;

fn schema_version(connection: &Connection) -> Result<i32> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn set_schema_version(connection: &Connection, version: i32) -> Result<()> {
    // `PRAGMA user_version = ?` takes no bound parameter, so the number is
    // formatted in. It is an i32 this file chose, never anything from outside.
    connection.execute_batch(&format!("PRAGMA user_version = {version}"))?;

    Ok(())
}

/// The version an unversioned database has already reached.
///
/// Databases written before the version existed report 0 however far they have
/// actually come, because the column-sniffing code this replaced left no record.
/// So the shape is inspected here, once, and the answer is written down — after
/// that the version decides and nothing looks at columns again.
fn adopt_version(connection: &Connection) -> Result<i32> {
    // The last thing each version added, newest first, so the newest marker found
    // is the version the database is at.
    const MARKERS: [(i32, &str, &str); 6] = [
        (6, "providers", "enabled"),
        (5, "providers", "last_attempt_at"),
        (4, "balance_snapshots", "provider_threshold_usd"),
        (3, "providers", "notified_error_kind"),
        (2, "balance_snapshots", "spend_window_days"),
        (1, "balance_snapshots", "key_fingerprint"),
    ];

    for (version, table, column) in MARKERS {
        if column_exists(connection, table, column)? {
            return Ok(version);
        }
    }

    Ok(0)
}

fn add_column(connection: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    connection
        .execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )
        .with_context(|| format!("could not add {table}.{column}"))?;

    Ok(())
}

/// Apply every step between `from` and the current version, in order.
///
/// Plain `ALTER`s rather than "add if missing" checks: once the version is
/// recorded it is the authority, so a step that cannot run is a real problem and
/// should say so rather than quietly pass.
fn migrate(connection: &Connection, from: i32) -> Result<()> {
    if from < 1 {
        // `basis` is what a remaining figure means: account credits, a key cap, or
        // spend. Without it a spend number was shown as money left.
        add_column(
            connection,
            "balance_snapshots",
            "basis",
            "TEXT NOT NULL DEFAULT 'usage'",
        )?;
        add_column(connection, "balance_snapshots", "key_fingerprint", "TEXT")?;

        // Rows written before `basis` existed. Account credits identify
        // themselves; everything else was either spend or a cap, and the two
        // cannot be told apart after the fact, so it keeps the label that claims
        // the least.
        connection.execute(
            "UPDATE balance_snapshots SET basis = 'account_credits' \
             WHERE basis = 'usage' AND account_credits IS NOT NULL",
            [],
        )?;
    }

    if from < 2 {
        // How many days a spend figure covers. CheaperInference's is always a
        // window and OpenRouter's is all-time, so the window travels with the
        // number: adding the two together would be meaningless.
        add_column(
            connection,
            "balance_snapshots",
            "spend_window_days",
            "INTEGER",
        )?;
    }

    if from < 3 {
        // Nullable, and null is the normal case: the provider's own threshold
        // where it publishes one, and the app default otherwise. Zero would mean
        // "never warn me", which is a different thing.
        add_column(connection, "providers", "low_balance_threshold", "REAL")?;
        // What the user was last told, so a notification is an edge rather than a
        // state. Zero rather than null deliberately: a provider already under its
        // threshold when the app first looks at it is news, and an initial
        // "unknown" would mean the crossing never happens and a fresh install
        // says nothing about a balance that was already low.
        add_column(
            connection,
            "providers",
            "notified_below",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        add_column(connection, "providers", "notified_error_kind", "TEXT")?;
    }

    if from < 4 {
        // What the provider itself called low at the time of the reading.
        add_column(
            connection,
            "balance_snapshots",
            "provider_threshold_usd",
            "REAL",
        )?;
    }

    if from < 5 {
        // How often this provider alone is checked, and when it was last asked.
        // The second has to be stored rather than held in memory: the poller
        // restarts with the app, and "is this due" must not reset on every
        // launch. It stays apart from `balance_snapshots.recorded_at`, which says
        // when a reading was stored and would be a different thing if a failed
        // check moved it.
        add_column(connection, "providers", "poll_interval_minutes", "INTEGER")?;
        add_column(connection, "providers", "last_attempt_at", "TEXT")?;
    }

    if from < 6 {
        // Whether the app tracks this provider at all. Rows that already exist
        // default to tracked, because they were being polled until now and
        // upgrading must not silently stop watching a balance.
        add_column(
            connection,
            "providers",
            "enabled",
            "INTEGER NOT NULL DEFAULT 1",
        )?;
    }

    Ok(())
}

fn initialize_database(connection: &Connection) -> Result<()> {
    // Deliberately the *original* schema, not the current one. Every column added
    // since lives in the ladder below, so a fresh database and a migrated one
    // walk the same steps and cannot drift apart. Put a new column here as well
    // and adoption would mistake a brand new file for a part-migrated one.
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS providers (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE
        );

        CREATE TABLE IF NOT EXISTS settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
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

    // Zero means nobody recorded a version: either a database from before the
    // version existed, which may be part way along, or a brand new one, which the
    // CREATE above has just brought fully up to date. Ask the shape once, write
    // the answer down, and let the version decide from then on.
    let recorded = schema_version(connection)?;

    if recorded > SCHEMA_VERSION {
        // A newer build has written this database. Change nothing and leave the
        // number alone: every column this build knows about is present, and
        // lowering the version would make the next upgrade re-run steps that have
        // already happened.
    } else {
        let from = if recorded == 0 {
            adopt_version(connection)?
        } else {
            recorded
        };

        migrate(connection, from)?;
        set_schema_version(connection, SCHEMA_VERSION)?;
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
            (provider_id, remaining, basis, account_credits, usage, spend_window_days,
             provider_threshold_usd, key_fingerprint)
        SELECT id, ?1, ?2, ?3, ?4, ?5, ?6, ?7 FROM providers WHERE name = ?8
        ",
        params![
            balance.remaining,
            balance.basis.as_str(),
            balance.account_credits,
            balance.usage,
            balance.spend_window_days,
            balance.provider_threshold,
            balance.key_fingerprint,
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
               balance_snapshots.remaining,
               balance_snapshots.provider_threshold_usd,
               balance_snapshots.key_fingerprint
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
            provider_threshold: row.get(6)?,
            key_fingerprint: row.get(7)?,
        })
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not read balance history")
}

const CSV_HEADER: &str = "provider,recorded_at,basis,remaining,account_credits,usage,\
spend_window_days,provider_threshold_usd,key_fingerprint";

/// One CSV field, quoted only when it has to be.
///
/// Nothing written today contains a comma, but a provider with one in its display
/// name would silently shift every later column by one, and a spreadsheet cannot
/// tell that a column is wrong.
fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn csv_number(value: Option<f64>) -> String {
    value.map_or_else(String::new, |value| value.to_string())
}

/// The reading history as CSV: one row per stored reading, oldest first.
///
/// `basis` and `spend_window_days` travel with every row deliberately. A spend
/// figure is not money left, and a 90-day spend is not an all-time one, so an
/// export that dropped them would hand someone a spreadsheet meaning the opposite
/// of what it looks like — the same mistake those columns exist to prevent. The
/// fingerprint is there for the same reason: without it, readings from two
/// different accounts look like one continuous series.
///
/// `only` limits the export to one provider; `None` covers all of them. Oldest
/// first, which is the order a spreadsheet wants, rather than the newest-first
/// order `history` returns for display.
pub fn history_csv(connection: &Connection, only: Option<&str>) -> Result<String> {
    let mut statement = connection.prepare(
        "SELECT providers.name,
                balance_snapshots.recorded_at,
                balance_snapshots.basis,
                balance_snapshots.remaining,
                balance_snapshots.account_credits,
                balance_snapshots.usage,
                balance_snapshots.spend_window_days,
                balance_snapshots.provider_threshold_usd,
                balance_snapshots.key_fingerprint
         FROM balance_snapshots
         JOIN providers ON providers.id = balance_snapshots.provider_id
         WHERE ?1 IS NULL OR providers.name = ?1
         ORDER BY balance_snapshots.recorded_at, balance_snapshots.id",
    )?;

    let rows = statement.query_map(params![only], |row| {
        Ok([
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, f64>(3)?.to_string(),
            csv_number(row.get(4)?),
            csv_number(row.get(5)?),
            row.get::<_, Option<u32>>(6)?
                .map_or_else(String::new, |days| days.to_string()),
            csv_number(row.get(7)?),
            row.get::<_, Option<String>>(8)?.unwrap_or_default(),
        ])
    })?;

    let mut csv = String::from(CSV_HEADER);

    for row in rows {
        let row = row?;
        csv.push('\n');
        csv.push_str(&row.map(|field| csv_field(&field)).join(","));
    }

    // A trailing newline, so appending to the file later still starts on its own
    // line.
    csv.push('\n');

    Ok(csv)
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
///
/// Returns the environment variable still supplying a key, if there is one. The
/// keychain entry is ours to delete; a variable in the caller's environment is
/// not, so removing the entry cannot on its own leave a provider unconfigured.
/// Returning it is what stops a removal from looking like it silently failed.
pub fn forget_key(provider: &str) -> Result<Option<&'static str>> {
    if !PROVIDERS.contains(&provider) {
        return Err(anyhow!(
            "unknown provider: {provider}; use {}",
            PROVIDERS.join(", ")
        ));
    }

    let Ok(entry) = Entry::new(KEYRING_SERVICE, provider) else {
        return Ok(env_credential(provider));
    };

    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(env_credential(provider)),
        Err(error) => Err(error).context("could not remove the key from the OS keychain"),
    }
}

/// Poll interval used until someone changes it, in minutes.
pub const DEFAULT_POLL_INTERVAL_MINUTES: u32 = 30;

/// Low-balance threshold used for any provider without its own.
pub const DEFAULT_LOW_BALANCE_THRESHOLD: f64 = 2.0;

/// Settings that apply to the whole app rather than to one provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// How often the background poller checks every provider.
    pub poll_interval_minutes: u32,
    /// Balance below which a provider counts as low, unless it has its own.
    pub low_balance_threshold: f64,
    /// Whether crossing a threshold is worth an OS notification.
    pub notify_low_balance: bool,
    /// Whether a credential that stops working is worth one.
    pub notify_key_errors: bool,
    /// Where CSV exports are written. `None` means the data directory, which is
    /// where the database already is and therefore never a surprise.
    ///
    /// This one can live in the database, unlike the database's own location:
    /// it is only ever read when the database is already open.
    #[serde(default)]
    pub export_directory: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            poll_interval_minutes: DEFAULT_POLL_INTERVAL_MINUTES,
            low_balance_threshold: DEFAULT_LOW_BALANCE_THRESHOLD,
            // On by default, because the entire point of polling in the
            // background is being told without having to go and look.
            notify_low_balance: true,
            notify_key_errors: true,
            export_directory: None,
        }
    }
}

const SETTING_POLL_INTERVAL: &str = "poll_interval_minutes";
const SETTING_LOW_THRESHOLD: &str = "low_balance_threshold";
const SETTING_NOTIFY_LOW: &str = "notify_low_balance";
const SETTING_NOTIFY_ERRORS: &str = "notify_key_errors";
const SETTING_EXPORT_DIRECTORY: &str = "export_directory";

/// Stored as `1` or `0`. Anything else is ignored rather than guessed at, so a
/// garbled row cannot quietly switch notifications off.
fn parse_flag(value: &str) -> Option<bool> {
    match value {
        "1" | "true" => Some(true),
        "0" | "false" => Some(false),
        _ => None,
    }
}

fn flag(value: bool) -> &'static str {
    if value { "1" } else { "0" }
}

/// Read the stored settings, falling back to the defaults.
///
/// A key/value table rather than a one-row table, so adding a setting does not
/// need a migration. Unknown keys are ignored rather than rejected, so this can
/// read a database written by a newer build.
pub fn load_settings(connection: &Connection) -> Result<Settings> {
    let mut settings = Settings::default();
    let mut statement = connection.prepare("SELECT key, value FROM settings")?;

    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    for row in rows {
        let (key, value) = row?;

        match key.as_str() {
            // Clamped, because a stored zero would turn the poller into a spin
            // loop against a paid API.
            SETTING_POLL_INTERVAL => {
                if let Ok(minutes) = value.parse::<u32>() {
                    settings.poll_interval_minutes = minutes.max(1);
                }
            }
            SETTING_LOW_THRESHOLD => {
                if let Ok(threshold) = value.parse::<f64>() {
                    settings.low_balance_threshold = threshold;
                }
            }
            SETTING_NOTIFY_LOW => {
                if let Some(on) = parse_flag(&value) {
                    settings.notify_low_balance = on;
                }
            }
            SETTING_NOTIFY_ERRORS => {
                if let Some(on) = parse_flag(&value) {
                    settings.notify_key_errors = on;
                }
            }
            SETTING_EXPORT_DIRECTORY => {
                // Blank means "wherever the database is", which is not a folder
                // named after the empty string.
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    settings.export_directory = Some(trimmed.to_string());
                }
            }
            _ => {}
        }
    }

    Ok(settings)
}

pub fn save_settings(connection: &Connection, settings: &Settings) -> Result<()> {
    let pairs = [
        (
            SETTING_POLL_INTERVAL,
            settings.poll_interval_minutes.max(1).to_string(),
        ),
        (
            SETTING_LOW_THRESHOLD,
            settings.low_balance_threshold.to_string(),
        ),
        (
            SETTING_NOTIFY_LOW,
            flag(settings.notify_low_balance).to_string(),
        ),
        (
            SETTING_NOTIFY_ERRORS,
            flag(settings.notify_key_errors).to_string(),
        ),
        (
            SETTING_EXPORT_DIRECTORY,
            settings.export_directory.clone().unwrap_or_default(),
        ),
    ];

    for (key, value) in pairs {
        connection.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
    }

    Ok(())
}

/// The providers the app is tracking, in the order they were added.
///
/// This is what the poller fetches, what the dashboard shows and what the tray
/// reports on. A provider that is switched off keeps its row, its key and its
/// readings; it is simply not polled and not shown.
pub fn tracked_providers(connection: &Connection) -> Result<Vec<String>> {
    let mut statement =
        connection.prepare("SELECT name FROM providers WHERE enabled = 1 ORDER BY id")?;

    let rows = statement.query_map([], |row| row.get(0))?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not read the tracked providers")
}

/// How many readings are stored for each provider.
///
/// Shown in the CLI's provider list, where it is the evidence for the promise that
/// switching a provider off keeps its history.
pub fn reading_counts(connection: &Connection) -> Result<Vec<(String, i64)>> {
    let mut statement = connection.prepare(
        "SELECT providers.name, count(balance_snapshots.id)
         FROM providers
         LEFT JOIN balance_snapshots ON balance_snapshots.provider_id = providers.id
         GROUP BY providers.id
         ORDER BY providers.id",
    )?;

    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not count the readings")
}

/// Whether each provider is tracked, for the screen that offers the switch.
///
/// Every row, unlike `tracked_providers`: the settings table has to be able to
/// show a provider that is switched off, or there would be no way to switch it
/// back on.
pub fn provider_enabled(connection: &Connection) -> Result<Vec<(String, bool)>> {
    let mut statement =
        connection.prepare("SELECT name, enabled FROM providers ORDER BY id")?;

    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0))
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not read which providers are tracked")
}

/// Switch a provider on or off without deleting its key or its history.
///
/// The reason this exists rather than "remove": a provider someone has stopped
/// using is one they may want back, and readings cannot be fetched again once
/// they are gone.
pub fn set_provider_enabled(connection: &Connection, provider: &str, enabled: bool) -> Result<()> {
    let changed = connection.execute(
        "UPDATE providers SET enabled = ?1 WHERE name = ?2",
        params![i64::from(enabled), provider],
    )?;

    if changed == 0 {
        return Err(anyhow!("no provider named {provider}"));
    }

    Ok(())
}

/// Each provider's own threshold, `None` where it has not been overridden.
pub fn provider_thresholds(connection: &Connection) -> Result<Vec<(String, Option<f64>)>> {
    let mut statement =
        connection.prepare("SELECT name, low_balance_threshold FROM providers ORDER BY id")?;

    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not read provider thresholds")
}

pub fn set_provider_threshold(
    connection: &Connection,
    provider: &str,
    threshold: Option<f64>,
) -> Result<()> {
    let changed = connection.execute(
        "UPDATE providers SET low_balance_threshold = ?1 WHERE name = ?2",
        params![threshold, provider],
    )?;

    if changed == 0 {
        return Err(anyhow!("no provider named {provider}"));
    }

    Ok(())
}

/// Each provider's own poll interval in minutes, `None` where it has not been
/// overridden.
pub fn provider_intervals(connection: &Connection) -> Result<Vec<(String, Option<u32>)>> {
    let mut statement =
        connection.prepare("SELECT name, poll_interval_minutes FROM providers ORDER BY id")?;

    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not read provider intervals")
}

pub fn set_provider_interval(
    connection: &Connection,
    provider: &str,
    minutes: Option<u32>,
) -> Result<()> {
    let changed = connection.execute(
        "UPDATE providers SET poll_interval_minutes = ?1 WHERE name = ?2",
        params![minutes, provider],
    )?;

    if changed == 0 {
        return Err(anyhow!("no provider named {provider}"));
    }

    Ok(())
}

/// The interval that applies to a provider: its own, or the app default.
///
/// Clamped at one minute for the same reason the stored default is: zero would
/// turn the poller into a spin loop against a paid API. A provider's own value
/// reaches the poller through here, so this is the one place that has to hold.
pub fn effective_interval(settings: &Settings, provider: Option<u32>) -> u32 {
    provider.unwrap_or(settings.poll_interval_minutes).max(1)
}

/// Every provider's effective interval, already resolved against the default.
pub fn resolved_intervals(connection: &Connection) -> Result<Vec<(String, u32)>> {
    let settings = load_settings(connection)?;

    Ok(provider_intervals(connection)?
        .into_iter()
        .map(|(name, own)| (name, effective_interval(&settings, own)))
        .collect())
}

/// Providers whose own interval has elapsed since they were last asked.
///
/// The comparison happens in SQLite, which is where the interval lives and which
/// already knows how to turn its own timestamps into epoch seconds. Doing it in
/// Rust would mean adding a date library for one subtraction.
///
/// A provider that has never been asked is due, which is what makes a fresh
/// install fetch without anyone pressing anything.
pub fn due_providers(connection: &Connection) -> Result<Vec<String>> {
    let settings = load_settings(connection)?;

    let mut statement = connection.prepare(
        "SELECT name FROM providers
         WHERE enabled = 1
           AND (last_attempt_at IS NULL
            OR strftime('%s', 'now') - strftime('%s', last_attempt_at)
               >= COALESCE(poll_interval_minutes, ?1) * 60)
         ORDER BY id",
    )?;

    // Straight out of the query, which orders by id. The set that used to be built
    // here existed only to filter these rows through the compiled registry, and
    // collecting into it threw away that order: a fetch would have covered the
    // providers in whatever order a hash map happened to produce.
    statement
        .query_map(params![settings.poll_interval_minutes], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()
        .context("could not read which providers are due")
}
/// Record that these providers were just asked.
///
/// Written before the fetch, not after it, so a provider that fails backs off for
/// its own interval instead of being retried on the next beat. A provider that is
/// rate-limiting is the case that matters.
/// Takes anything string-like so a caller with `&[String]` and a test with
/// `&["openrouter"]` both work without one of them building a vector to be read.
pub fn record_attempts<S: AsRef<str>>(connection: &Connection, providers: &[S]) -> Result<()> {
    for provider in providers {
        connection.execute(
            "UPDATE providers SET last_attempt_at = CURRENT_TIMESTAMP WHERE name = ?1",
            params![provider.as_ref()],
        )?;
    }

    Ok(())
}

/// When each provider was last asked, or `None` where it never has been.
pub fn last_attempts(connection: &Connection) -> Result<Vec<(String, Option<String>)>> {
    let mut statement =
        connection.prepare("SELECT name, last_attempt_at FROM providers ORDER BY id")?;

    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not read when each provider was last asked")
}

/// The threshold that applies to a provider.
///
/// The order is: the number the user set for this provider, then the number the
/// provider publishes for itself, then the app-wide default. The middle step is
/// what stops a flat $2 from overriding a provider that knows its own account
/// better — CheaperInference says which balance it auto-recharges at.
///
/// The tray, the dashboard and the notifier all go through this, so a provider
/// cannot be low in one place and fine in the other.
pub fn effective_threshold(settings: &Settings, own: Option<f64>, reported: Option<f64>) -> f64 {
    own.or(reported).unwrap_or(settings.low_balance_threshold)
}

/// When each provider's history starts, or `None` where it has no readings yet.
///
/// Derived from the snapshots rather than stored as a column on `providers`. The
/// earliest reading is the fact the database actually holds; a timestamp written
/// when the row was inserted would be a second, weaker copy of it, and it could
/// say nothing at all about a provider that was already configured the first time
/// the app ran — which is every existing install.
pub fn first_reading_at(connection: &Connection) -> Result<Vec<(String, Option<String>)>> {
    let mut statement = connection.prepare(
        "SELECT providers.name, min(balance_snapshots.recorded_at)
         FROM providers
         LEFT JOIN balance_snapshots ON balance_snapshots.provider_id = providers.id
         GROUP BY providers.id
         ORDER BY providers.id",
    )?;

    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not read when each provider's history starts")
}

/// Each provider's own published threshold, as of its latest reading.
///
/// `None` where the provider has never reported one, or has stopped.
///
/// ponytail: this reads the newest reading regardless of which credential
/// produced it, so a threshold belonging to a swapped-out account lingers until
/// the next successful fetch replaces it. Filtering on the current fingerprint
/// would put a keychain read inside a function the tests call, and one poll of
/// staleness is not worth making those tests machine-dependent.
pub fn reported_thresholds(connection: &Connection) -> Result<Vec<(String, Option<f64>)>> {
    let mut statement = connection.prepare(
        "SELECT providers.name,
                (SELECT balance_snapshots.provider_threshold_usd
                 FROM balance_snapshots
                 WHERE balance_snapshots.provider_id = providers.id
                 ORDER BY balance_snapshots.recorded_at DESC, balance_snapshots.id DESC
                 LIMIT 1)
         FROM providers
         ORDER BY providers.id",
    )?;

    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("could not read provider thresholds")
}

/// A provider's threshold in the two halves the rule needs: what the user chose
/// for it, and what the provider says about itself.
type ThresholdRow = (String, Option<f64>, Option<f64>);

fn threshold_rows(connection: &Connection) -> Result<Vec<ThresholdRow>> {
    let reported: HashMap<String, Option<f64>> =
        reported_thresholds(connection)?.into_iter().collect();

    Ok(provider_thresholds(connection)?
        .into_iter()
        .map(|(name, own)| {
            let reported = reported.get(&name).copied().flatten();
            (name, own, reported)
        })
        .collect())
}

/// Every tracked provider's effective threshold, already resolved against the
/// default. A provider that is switched off is left out, so the tray and the
/// notifier cannot report on something the app is not watching.
pub fn resolved_thresholds(connection: &Connection) -> Result<Vec<(String, f64)>> {
    let settings = load_settings(connection)?;
    // Read once rather than asking per row: this is a handful either way, but the
    // shape keeps it one query as providers are added.
    let tracked: HashSet<String> = tracked_providers(connection)?.into_iter().collect();

    Ok(threshold_rows(connection)?
        .into_iter()
        .filter(|(name, _, _)| tracked.contains(name))
        .map(|(name, own, reported)| (name, effective_threshold(&settings, own, reported)))
        .collect())
}

/// The number that applies to each provider when it has no override of its own.
///
/// The settings screen shows this as the placeholder in a blank box, so that a
/// blank box means in the form exactly what `effective_threshold` does at poll
/// time. For CheaperInference that is its own auto-recharge threshold, which is
/// not the app default.
pub fn fallback_thresholds(connection: &Connection) -> Result<Vec<(String, f64)>> {
    let settings = load_settings(connection)?;

    Ok(threshold_rows(connection)?
        .into_iter()
        .map(|(name, _, reported)| (name, effective_threshold(&settings, None, reported)))
        .collect())
}

/// Something worth interrupting the user about.
///
/// Structured rather than pre-worded, so the copy stays with the surface that
/// shows it. Tests then assert on what happened rather than on a sentence, which
/// does not have to be rewritten when the wording changes.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum Notice {
    LowBalance {
        provider: String,
        display_name: String,
        remaining: f64,
        threshold: f64,
    },
    KeyError {
        provider: String,
        display_name: String,
        /// `ProviderError::kind()`, so the app can choose the right sentence
        /// without parsing the message back out again. Named to match
        /// `RefreshOutcome`, which carries the same thing.
        error_kind: String,
        message: String,
    },
}

/// Work out which notices are due, and record that they have been sent.
///
/// "Take" rather than "check", because it consumes the edge instead of only
/// describing it. A provider sitting below its threshold does not notify on every
/// check, only on the one where it crossed; a broken key does not notify on every
/// poll. The state lives in the database, so closing and reopening the app does
/// not replay a warning for a balance that has been low for days.
///
/// A provider that fails to report is left exactly as it was. Its balance is
/// unknown, and unknown is not the same as fine: recording a blip as "no longer
/// low" would re-fire the warning the moment the balance became readable again.
pub fn take_notifications(
    connection: &Connection,
    outcomes: &[Outcome],
    settings: &Settings,
) -> Result<Vec<Notice>> {
    // Resolved against the settings passed in, never the ones stored. The caller
    // may have just changed a threshold and not saved it yet, and a notification
    // has to use the threshold the user is actually looking at.
    let own: HashMap<String, Option<f64>> = provider_thresholds(connection)?.into_iter().collect();
    let reported: HashMap<String, Option<f64>> =
        reported_thresholds(connection)?.into_iter().collect();

    let mut notices = Vec::new();

    for (name, result) in outcomes {
        // Borrowed, not copied: the id is owned by the outcome now.
        let name = name.as_str();

        let previous = connection
            .query_row(
                "SELECT notified_below, notified_error_kind FROM providers WHERE name = ?1",
                params![name],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;

        // A provider the database has never heard of is not something to guess
        // about; a notice naming the wrong state is worse than none.
        let Some((was_below, told_about_error)) = previous else {
            continue;
        };

        match result {
            Ok(balance) => {
                let threshold = effective_threshold(
                    settings,
                    own.get(name).copied().flatten(),
                    reported.get(name).copied().flatten(),
                );

                // A spend figure is not money left, so comparing one to a
                // threshold would warn about a number that means the opposite.
                let is_below = balance.basis.is_balance() && balance.remaining < threshold;

                if settings.notify_low_balance && is_below && was_below == 0 {
                    notices.push(Notice::LowBalance {
                        provider: name.to_string(),
                        display_name: display_name(name).to_string(),
                        remaining: balance.remaining,
                        threshold,
                    });
                }

                // A successful read clears the error, so a key that breaks again
                // later is news again instead of being suppressed by an old
                // notice that nobody remembers seeing.
                connection.execute(
                    "UPDATE providers SET notified_below = ?1, notified_error_kind = NULL \
                     WHERE name = ?2",
                    params![i64::from(is_below), name],
                )?;
            }
            Err(error) => {
                if settings.notify_key_errors
                    && error.credential_is_broken()
                    && told_about_error.as_deref() != Some(error.kind())
                {
                    notices.push(Notice::KeyError {
                        provider: name.to_string(),
                        display_name: display_name(name).to_string(),
                        error_kind: error.kind().to_string(),
                        message: error.to_string(),
                    });

                    // Only credential failures are recorded. Writing this on a
                    // network blip would erase the memory of a real warning and
                    // let the next poll repeat it.
                    connection.execute(
                        "UPDATE providers SET notified_error_kind = ?1 WHERE name = ?2",
                        params![error.kind(), name],
                    )?;
                }
            }
        }
    }

    Ok(notices)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One provider's result, so a test says which provider and what happened
    /// rather than repeating the tuple the fetch path happens to use.
    fn outcome(provider: &str, result: Result<Balance, ProviderError>) -> Outcome {
        (provider.to_string(), result)
    }

    /// Every provider this build knows, as the database lists them.
    ///
    /// Tests say this rather than a count: adding a provider should not mean
    /// hunting for a `2` that used to be right.
    fn every_provider() -> Vec<String> {
        PROVIDERS.iter().map(|name| (*name).to_string()).collect()
    }

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
            provider: "openrouter".to_string(),
            basis,
            remaining,
            account_credits: credits,
            usage,
            spend_window_days: None,
            provider_threshold: None,
            key_fingerprint: None,
        }
    }

    fn ok(remaining: f64) -> Result<Balance, ProviderError> {
        Ok(balance(
            Basis::AccountCredits,
            remaining,
            Some(remaining),
            None,
        ))
    }

    fn low_settings(threshold: f64) -> Settings {
        Settings {
            low_balance_threshold: threshold,
            ..Settings::default()
        }
    }

    #[test]
    fn a_provider_above_its_threshold_is_never_announced() {
        let connection = memory_database();

        let notices = take_notifications(
            &connection,
            &[outcome("openrouter", ok(50.0))],
            &low_settings(10.0),
        )
        .expect("notices");

        assert!(notices.is_empty());
    }

    #[test]
    fn a_crossing_is_announced_once_and_then_not_again() {
        let connection = memory_database();
        let settings = low_settings(10.0);

        let first = take_notifications(&connection, &[outcome("openrouter", ok(6.4))], &settings)
            .expect("notices");
        assert_eq!(
            first,
            vec![Notice::LowBalance {
                provider: "openrouter".into(),
                display_name: "OpenRouter".into(),
                remaining: 6.4,
                threshold: 10.0,
            }]
        );

        // Still low on the next check, but the user has already been told.
        assert!(
            take_notifications(&connection, &[outcome("openrouter", ok(6.4))], &settings)
                .expect("notices")
                .is_empty()
        );

        // Topped up, then crossed down again: that is a new event.
        take_notifications(&connection, &[outcome("openrouter", ok(25.0))], &settings).expect("notices");
        assert_eq!(
            take_notifications(&connection, &[outcome("openrouter", ok(6.4))], &settings)
                .expect("notices")
                .len(),
            1
        );
    }

    #[test]
    fn a_provider_that_is_already_low_is_announced_on_the_first_check() {
        // The fresh-install case. Announcing only on a crossing would say nothing
        // at all here, which is the one time it is most needed.
        let connection = memory_database();

        let notices =
            take_notifications(&connection, &[outcome("openrouter", ok(6.4))], &low_settings(10.0))
                .expect("notices");

        assert_eq!(notices.len(), 1);
    }

    #[test]
    fn a_broken_credential_is_announced_once_per_outage() {
        let connection = memory_database();
        let settings = low_settings(2.0);

        let first = take_notifications(
            &connection,
            &[outcome("openrouter", Err(ProviderError::Unauthorized))],
            &settings,
        )
        .expect("notices");
        assert!(matches!(first[..], [Notice::KeyError { .. }]));

        // Still broken, and it has already said so.
        assert!(
            take_notifications(
                &connection,
                &[outcome("openrouter", Err(ProviderError::Unauthorized))],
                &settings,
            )
            .expect("notices")
            .is_empty()
        );

        // A working key clears the memory, so breaking again is news again.
        take_notifications(&connection, &[outcome("openrouter", ok(50.0))], &settings).expect("notices");
        assert_eq!(
            take_notifications(
                &connection,
                &[outcome("openrouter", Err(ProviderError::Unauthorized))],
                &settings,
            )
            .expect("notices")
            .len(),
            1
        );
    }

    #[test]
    fn a_network_failure_does_not_claim_the_key_is_broken() {
        // Sending someone to rotate a key that was fine, because their wifi
        // dropped, is worse than staying quiet.
        let connection = memory_database();

        let notices = take_notifications(
            &connection,
            &[outcome("openrouter", Err(ProviderError::Unreachable("dns".into())))],
            &low_settings(10.0),
        )
        .expect("notices");

        assert!(notices.is_empty());
    }

    #[test]
    fn a_failed_check_does_not_clear_a_crossing_already_announced() {
        let connection = memory_database();
        let settings = low_settings(10.0);

        take_notifications(&connection, &[outcome("openrouter", ok(6.4))], &settings).expect("notices");

        // Unknown is not the same as fine. Recording the blip as "no longer low"
        // would re-fire the warning as soon as the balance was readable again.
        take_notifications(
            &connection,
            &[outcome("openrouter", Err(ProviderError::Unreachable("dns".into())))],
            &settings,
        )
        .expect("notices");

        assert!(
            take_notifications(&connection, &[outcome("openrouter", ok(6.4))], &settings)
                .expect("notices")
                .is_empty()
        );
    }

    #[test]
    fn a_spend_reading_is_not_mistaken_for_a_low_balance() {
        let connection = memory_database();

        // 6.4 of spend against a 10 threshold is not 6.4 of money left.
        let notices = take_notifications(
            &connection,
            &[outcome(
                "openrouter",
                Ok(balance(Basis::Usage, 6.4, None, Some(6.4))),
            )],
            &low_settings(10.0),
        )
        .expect("notices");

        assert!(notices.is_empty());
    }

    #[test]
    fn switching_the_notifications_off_silences_both_kinds() {
        let connection = memory_database();
        let settings = Settings {
            low_balance_threshold: 10.0,
            notify_low_balance: false,
            notify_key_errors: false,
            ..Settings::default()
        };

        let notices = take_notifications(
            &connection,
            &[
                outcome("openrouter", ok(6.4)),
                outcome("cheaperinference", Err(ProviderError::Unauthorized)),
            ],
            &settings,
        )
        .expect("notices");

        assert!(notices.is_empty());
    }

    #[test]
    fn each_provider_is_measured_against_its_own_threshold() {
        let connection = memory_database();
        set_provider_threshold(&connection, "cheaperinference", Some(50.0)).expect("threshold");

        let notices = take_notifications(
            &connection,
            &[outcome("openrouter", ok(6.4)), outcome("cheaperinference", ok(11.59))],
            &low_settings(10.0),
        )
        .expect("notices");

        // CheaperInference is under the 50 it was given, and would have looked
        // perfectly healthy measured against the app default.
        assert_eq!(notices.len(), 2);
        assert!(matches!(&notices[0], Notice::LowBalance { threshold, .. } if *threshold == 10.0));
        assert!(matches!(&notices[1], Notice::LowBalance { threshold, .. } if *threshold == 50.0));
    }

    #[test]
    fn notification_choices_survive_a_restart() {
        let connection = memory_database();
        let settings = Settings {
            notify_low_balance: false,
            notify_key_errors: false,
            ..Settings::default()
        };

        save_settings(&connection, &settings).expect("saved");
        let reloaded = load_settings(&connection).expect("loaded");

        assert!(!reloaded.notify_low_balance);
        assert!(!reloaded.notify_key_errors);
    }

    #[test]
    fn account_credits_win_over_a_cap_and_over_usage() {
        let info = key_info(Some(50.0), Some(7.5));
        let resolved = openrouter_balance("openrouter", &info, Some(20.0)).expect("a balance");

        assert_eq!(resolved.basis, Basis::AccountCredits);
        assert_eq!(resolved.remaining, 20.0);
        assert!(resolved.basis.is_balance());
        assert_eq!(resolved.usage, Some(7.5));
    }

    #[test]
    fn a_key_cap_is_used_when_credits_are_unreadable() {
        let info = key_info(Some(50.0), Some(7.5));
        let resolved = openrouter_balance("openrouter", &info, None).expect("a balance");

        assert_eq!(resolved.basis, Basis::KeyCap);
        assert_eq!(resolved.remaining, 50.0);
        assert!(resolved.basis.is_balance());
        assert_eq!(resolved.account_credits, None);
    }

    #[test]
    fn usage_is_the_last_resort_and_is_not_a_balance() {
        let info = key_info(None, Some(7.5));
        let resolved = openrouter_balance("openrouter", &info, None).expect("a balance");

        assert_eq!(resolved.basis, Basis::Usage);
        assert_eq!(resolved.remaining, 7.5);
        assert!(!resolved.basis.is_balance());
    }

    #[test]
    fn a_response_with_no_numbers_at_all_is_an_error() {
        let info = key_info(None, None);
        assert!(openrouter_balance("openrouter", &info, None).is_err());
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
    fn a_fingerprint_is_stable_and_carries_nothing_of_the_key() {
        let key = "sk-or-v1-abcdefghijklmnopqrstuvwxyz0123456789";
        let fingerprint = key_fingerprint(key);

        assert_eq!(fingerprint.len(), 16, "8 bytes written as hex");
        assert!(fingerprint.chars().all(|c| c.is_ascii_hexdigit()));

        // Stable, so every reading from one key groups together.
        assert_eq!(fingerprint, key_fingerprint(key));
        // A different key gets a different label.
        assert_ne!(fingerprint, key_fingerprint("sk-or-v1-something-else"));
        // And none of the key survives the trip.
        assert!(!fingerprint.contains("abcdef"));
        assert!(!key.contains(&fingerprint));
    }

    #[test]
    fn snapshots_remember_which_key_produced_them() {
        let connection = memory_database();

        // Two readings, two accounts, one provider.
        for (key, value) in [("key-one", 6.4), ("key-two", 12.0)] {
            let mut reading = balance(Basis::AccountCredits, value, Some(value), None);
            reading.key_fingerprint = Some(key_fingerprint(key));
            save_snapshot(&connection, &reading).expect("saved");
        }

        let rows = history(&connection, "openrouter", 10).expect("history");
        assert_eq!(rows.len(), 2);

        // This is the whole point: without it a chart draws one line through
        // two different accounts and quietly reports it as one balance.
        assert!(rows[0].key_fingerprint.is_some());
        assert_ne!(rows[0].key_fingerprint, rows[1].key_fingerprint);
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
                provider: "cheaperinference".to_string(),
                basis: Basis::AccountCredits,
                remaining: 99.0,
                account_credits: Some(99.0),
                usage: None,
                spend_window_days: None,
                provider_threshold: None,
                key_fingerprint: None,
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
        orphan.provider = "nowhere".to_string();

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

    fn columns_of(connection: &Connection, table: &str) -> Vec<String> {
        let mut statement = connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("table info");
        let rows = statement
            .query_map([], |row| row.get::<_, String>(1))
            .expect("column names");

        rows.collect::<rusqlite::Result<Vec<_>>>().expect("read")
    }

    /// A fresh database and one that walked the ladder from the original schema
    /// have to end up identical, or one of the two paths is wrong. This is what
    /// catches a column added to `CREATE TABLE` and forgotten in the ladder — or
    /// the other way round, which is how a new file gets mistaken for a
    /// part-migrated one.
    #[test]
    fn a_fresh_database_and_a_migrated_one_end_up_the_same() {
        let migrated = Connection::open_in_memory().expect("in-memory");
        migrated
            .execute_batch(
                "
                CREATE TABLE providers (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL UNIQUE
                );
                CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE balance_snapshots (
                    id INTEGER PRIMARY KEY,
                    provider_id INTEGER NOT NULL REFERENCES providers(id),
                    remaining REAL NOT NULL,
                    account_credits REAL,
                    usage REAL,
                    recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );
                ",
            )
            .expect("the original schema");
        initialize_database(&migrated).expect("migrated");

        let fresh = memory_database();

        for table in ["providers", "settings", "balance_snapshots"] {
            assert_eq!(
                columns_of(&migrated, table),
                columns_of(&fresh, table),
                "{table} differs between a fresh database and a migrated one"
            );
        }

        assert_eq!(schema_version(&migrated).expect("version"), SCHEMA_VERSION);
        assert_eq!(schema_version(&fresh).expect("version"), SCHEMA_VERSION);
    }

    /// Every step is reachable from nothing, and lands on the current version
    /// with the columns it promises. A step added without raising
    /// `SCHEMA_VERSION` would leave its column missing here.
    #[test]
    fn every_step_of_the_ladder_is_reachable() {
        let connection = memory_database();

        assert_eq!(schema_version(&connection).expect("version"), SCHEMA_VERSION);

        for (table, column) in [
            ("balance_snapshots", "basis"),
            ("balance_snapshots", "key_fingerprint"),
            ("balance_snapshots", "spend_window_days"),
            ("balance_snapshots", "provider_threshold_usd"),
            ("providers", "low_balance_threshold"),
            ("providers", "notified_below"),
            ("providers", "notified_error_kind"),
            ("providers", "poll_interval_minutes"),
            ("providers", "last_attempt_at"),
            ("providers", "enabled"),
        ] {
            assert!(
                column_exists(&connection, table, column).expect("checked"),
                "{table}.{column} is missing after the ladder ran"
            );
        }
    }

    /// An unversioned database that is already part way along is adopted at the
    /// shape it has, not at zero. Without this every existing install would have
    /// its migration steps re-run against columns that are already there.
    #[test]
    fn an_unversioned_database_is_adopted_at_the_shape_it_has() {
        let connection = Connection::open_in_memory().expect("in-memory");
        connection
            .execute_batch(
                "
                CREATE TABLE providers (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
                CREATE TABLE balance_snapshots (
                    id INTEGER PRIMARY KEY,
                    provider_id INTEGER NOT NULL REFERENCES providers(id),
                    remaining REAL NOT NULL,
                    basis TEXT NOT NULL,
                    account_credits REAL,
                    usage REAL,
                    key_fingerprint TEXT,
                    recorded_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                );
                ",
            )
            .expect("a version 1 shape");

        assert_eq!(adopt_version(&connection).expect("adopted"), 1);

        initialize_database(&connection).expect("migrated");
        assert_eq!(schema_version(&connection).expect("version"), SCHEMA_VERSION);
        // Only the steps after 1 ran, and they ran once.
        assert!(column_exists(&connection, "providers", "last_attempt_at").expect("checked"));
    }

    /// A database a newer build has written is left alone. Lowering the number
    /// would make the next upgrade re-run steps that have already happened.
    #[test]
    fn a_version_from_a_newer_build_is_not_lowered() {
        let connection = memory_database();
        set_schema_version(&connection, 9).expect("pretend a newer build wrote it");

        initialize_database(&connection).expect("reopened");

        assert_eq!(schema_version(&connection).expect("version"), 9);
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
        windowed.provider = "cheaperinference".to_string();
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
    fn display_names_keep_their_internal_capitals() {
        assert_eq!(display_name("openrouter"), "OpenRouter");
        assert_eq!(display_name("cheaperinference"), "CheaperInference");
        // Unknown providers come back unchanged rather than disappearing.
        assert_eq!(display_name("something"), "something");
    }

    #[test]
    fn a_key_hint_shows_the_format_and_the_last_four_only() {
        assert_eq!(mask_key("sk-or-v1-0123456789abcdef3f2a"), "sk-or-v1…3f2a");
        assert_eq!(mask_key("ci_abcdef123456"), "ci_abcde…3456");
        // Too short to show anything without giving it away.
        assert_eq!(mask_key("ci_short"), "••••••••");
        assert_eq!(mask_key(""), "••••••••");
    }

    #[test]
    fn a_key_is_recognised_by_its_format_prefix() {
        assert_eq!(provider_for_key("sk-or-v1-abc"), Some("openrouter"));
        assert_eq!(provider_for_key("ci_live_abc"), Some("cheaperinference"));
        assert_eq!(provider_for_key("something-else"), None);
    }

    #[test]
    fn a_misfiled_key_is_pointed_at_the_right_provider() {
        // The case that actually happened: a CheaperInference key pasted while
        // OpenRouter was selected, which comes back as "rejected".
        let hint = misdirected_key_hint("openrouter", "ci_live_abc").expect("a hint");
        assert!(hint.contains("CheaperInference"), "{hint}");

        // Correctly filed, so there is nothing to say.
        assert!(misdirected_key_hint("openrouter", "sk-or-v1-abc").is_none());
        // Unrecognised format, so nothing to say either.
        assert!(misdirected_key_hint("openrouter", "nonsense").is_none());
    }

    #[test]
    fn fragments_join_into_sentences() {
        assert_eq!(
            as_sentence("the API key was rejected"),
            "The API key was rejected."
        );
        // Already punctuated, so it is left alone.
        assert_eq!(as_sentence("it failed."), "It failed.");
        assert_eq!(as_sentence(""), "");
    }

    #[test]
    fn save_statuses_are_stable_identifiers() {
        let balance = balance(Basis::AccountCredits, 6.4, Some(6.4), Some(3.6));

        assert_eq!(
            SaveOutcome::Verified(balance.clone()).status(),
            "saved_verified"
        );
        assert_eq!(SaveOutcome::Verified(balance).balance(), Some(6.4));

        let unverified = SaveOutcome::SavedUnverified("offline".to_string());
        assert_eq!(unverified.status(), "saved_unverified");
        assert_eq!(unverified.balance(), None);
        assert_eq!(unverified.message(), Some("offline"));

        let rejected = SaveOutcome::Rejected("refused".to_string());
        assert_eq!(rejected.status(), "rejected");
        assert_eq!(rejected.balance(), None);
        // A stored key is only ever left alone in this case.
        assert_eq!(rejected.message(), Some("refused"));
    }

    #[test]
    fn settings_fall_back_to_their_defaults() {
        let connection = memory_database();
        let settings = load_settings(&connection).expect("settings");

        assert_eq!(
            settings.poll_interval_minutes,
            DEFAULT_POLL_INTERVAL_MINUTES
        );
        assert_eq!(
            settings.low_balance_threshold,
            DEFAULT_LOW_BALANCE_THRESHOLD
        );
    }

    #[test]
    fn settings_survive_a_round_trip() {
        let connection = memory_database();

        save_settings(
            &connection,
            &Settings {
                poll_interval_minutes: 15,
                low_balance_threshold: 7.5,
                ..Settings::default()
            },
        )
        .expect("saved");

        let settings = load_settings(&connection).expect("settings");
        assert_eq!(settings.poll_interval_minutes, 15);
        assert_eq!(settings.low_balance_threshold, 7.5);

        // Saving again overwrites rather than duplicating.
        save_settings(&connection, &Settings::default()).expect("saved");
        let settings = load_settings(&connection).expect("settings");
        assert_eq!(
            settings.poll_interval_minutes,
            DEFAULT_POLL_INTERVAL_MINUTES
        );

        // One row per setting, not one per save. Measured against itself rather
        // than a fixed number, so adding a setting does not break this test.
        let count = |connection: &Connection| -> i64 {
            connection
                .query_row("SELECT count(*) FROM settings", [], |row| row.get(0))
                .expect("count")
        };
        let before = count(&connection);
        save_settings(&connection, &Settings::default()).expect("saved a third time");
        assert_eq!(before, count(&connection), "saving again must not add rows");
        assert!(before > 0, "every setting should have written a row");
    }

    #[test]
    fn a_stored_interval_of_zero_is_clamped() {
        // Zero would turn the poller into a spin loop against a paid API.
        let connection = memory_database();
        connection
            .execute(
                "INSERT INTO settings (key, value) VALUES ('poll_interval_minutes', '0')",
                [],
            )
            .expect("stored");

        assert_eq!(
            load_settings(&connection)
                .expect("settings")
                .poll_interval_minutes,
            1
        );
    }

    #[test]
    fn unknown_settings_keys_are_ignored() {
        // A newer build may have written keys this one has never heard of.
        let connection = memory_database();
        connection
            .execute(
                "INSERT INTO settings (key, value) VALUES ('theme', 'midnight')",
                [],
            )
            .expect("stored");

        let settings = load_settings(&connection).expect("settings");
        assert_eq!(
            settings.poll_interval_minutes,
            DEFAULT_POLL_INTERVAL_MINUTES
        );
    }

    #[test]
    fn a_provider_threshold_overrides_the_default() {
        let connection = memory_database();
        let settings = Settings {
            poll_interval_minutes: 30,
            low_balance_threshold: 2.0,
            ..Settings::default()
        };

        assert_eq!(effective_threshold(&settings, None, None), 2.0);
        assert_eq!(effective_threshold(&settings, Some(5.0), None), 5.0);
        // The provider's own number is between the two: it beats the flat
        // default, and losing to an explicit choice is the point of the order.
        assert_eq!(effective_threshold(&settings, None, Some(4.0)), 4.0);
        assert_eq!(effective_threshold(&settings, Some(5.0), Some(4.0)), 5.0);

        set_provider_threshold(&connection, "openrouter", Some(9.0)).expect("stored");
        let thresholds = provider_thresholds(&connection).expect("thresholds");
        assert_eq!(
            thresholds.iter().find(|(name, _)| name == "openrouter"),
            Some(&("openrouter".to_string(), Some(9.0)))
        );

        // Clearing it goes back to the default rather than to zero.
        set_provider_threshold(&connection, "openrouter", None).expect("cleared");
        let thresholds = provider_thresholds(&connection).expect("thresholds");
        assert_eq!(
            thresholds.iter().find(|(name, _)| name == "openrouter"),
            Some(&("openrouter".to_string(), None))
        );

        assert!(set_provider_threshold(&connection, "nowhere", Some(1.0)).is_err());

        // The resolved view is what the tray and the dashboard actually use.
        set_provider_threshold(&connection, "cheaperinference", Some(7.0)).expect("stored");
        let resolved = resolved_thresholds(&connection).expect("resolved");
        assert_eq!(
            resolved.iter().find(|(name, _)| name == "openrouter"),
            Some(&("openrouter".to_string(), 2.0))
        );
        assert_eq!(
            resolved.iter().find(|(name, _)| name == "cheaperinference"),
            Some(&("cheaperinference".to_string(), 7.0))
        );
    }

    /// CheaperInference publishes the balance it auto-recharges at, which knows
    /// more about the account than a flat $2 does. The number has to survive a
    /// round trip through the database, because the tray and the notifier resolve
    /// thresholds from stored state rather than by re-fetching.
    #[test]
    fn a_providers_own_threshold_beats_the_app_default() {
        let connection = memory_database();

        let mut reading = balance(Basis::AccountCredits, 40.0, Some(40.0), None);
        reading.provider = "cheaperinference".to_string();
        reading.provider_threshold = Some(25.0);
        save_snapshot(&connection, &reading).expect("saved");

        let resolved = resolved_thresholds(&connection).expect("resolved");
        assert_eq!(
            resolved.iter().find(|(name, _)| name == "cheaperinference"),
            Some(&("cheaperinference".to_string(), 25.0))
        );
        // OpenRouter publishes nothing, so it keeps the app default.
        assert_eq!(
            resolved.iter().find(|(name, _)| name == "openrouter"),
            Some(&("openrouter".to_string(), DEFAULT_LOW_BALANCE_THRESHOLD))
        );

        // An explicit choice still wins over the provider's own number.
        set_provider_threshold(&connection, "cheaperinference", Some(7.0)).expect("stored");
        let resolved = resolved_thresholds(&connection).expect("resolved");
        assert_eq!(
            resolved.iter().find(|(name, _)| name == "cheaperinference"),
            Some(&("cheaperinference".to_string(), 7.0))
        );

        // Clearing it falls back to the provider again rather than to $2, and the
        // newest reading replaces the older one, so a provider that changes what
        // it calls low is followed instead of remembered.
        set_provider_threshold(&connection, "cheaperinference", None).expect("cleared");
        let mut newer = reading.clone();
        newer.provider_threshold = Some(30.0);
        save_snapshot(&connection, &newer).expect("saved");

        let resolved = resolved_thresholds(&connection).expect("resolved");
        assert_eq!(
            resolved.iter().find(|(name, _)| name == "cheaperinference"),
            Some(&("cheaperinference".to_string(), 30.0))
        );
    }

    /// The due check is what decides whether a paid endpoint gets called, so it
    /// is measured rather than assumed: a provider's own interval has to win over
    /// the app default, a provider that was never asked has to be due, and a
    /// failed attempt has to back off instead of being retried on the next beat.
    #[test]
    fn only_providers_whose_own_interval_has_elapsed_are_due() {
        let connection = memory_database();
        let settings = Settings {
            poll_interval_minutes: 30,
            ..Settings::default()
        };
        save_settings(&connection, &settings).expect("settings");

        // Never asked: due, which is what makes a fresh install fetch.
        assert_eq!(due_providers(&connection).expect("due"), PROVIDERS.to_vec());

        // OpenRouter on a four-hour interval; everything else stays on the app
        // default, so the assertion says which one moved rather than listing the
        // whole table.
        set_provider_interval(&connection, "openrouter", Some(240)).expect("stored");
        let intervals = resolved_intervals(&connection).expect("resolved");
        assert_eq!(
            intervals.iter().find(|(name, _)| name == "openrouter"),
            Some(&("openrouter".to_string(), 240))
        );
        assert!(intervals
            .iter()
            .filter(|(name, _)| name != "openrouter")
            .all(|(_, minutes)| *minutes == 30));

        record_attempts(&connection, &PROVIDERS).expect("recorded");
        assert!(
            due_providers(&connection).expect("due").is_empty(),
            "nothing is due straight after being asked"
        );

        // An attempt an hour ago: the app-default providers are due again and the
        // four-hour one is not. Backdating the row is how the clock is moved here,
        // since the comparison itself is SQLite's.
        connection
            .execute(
                "UPDATE providers SET last_attempt_at = datetime('now', '-1 hour')",
                [],
            )
            .expect("backdated");

        let due = due_providers(&connection).expect("due");
        assert!(!due.iter().any(|name| name == "openrouter"), "four hours have not passed");
        assert!(due.iter().any(|name| name == "cheaperinference"), "thirty minutes have");

        // Clearing the override puts it back on the app default, and zero is
        // clamped so it cannot become a spin loop against a paid API.
        set_provider_interval(&connection, "openrouter", None).expect("cleared");
        assert_eq!(effective_interval(&settings, Some(0)), 1);
        assert_eq!(effective_interval(&settings, None), 30);
        assert_eq!(effective_interval(&settings, Some(15)), 15);
    }

    /// A failed attempt still counts as an attempt, or a rate-limited provider
    /// would be hammered on every beat.
    #[test]
    fn an_attempt_is_recorded_even_when_nothing_is_read() {
        let connection = memory_database();

        record_attempts(&connection, &["openrouter"]).expect("recorded");

        let attempts = last_attempts(&connection).expect("attempts");
        let openrouter = attempts.iter().find(|(name, _)| name == "openrouter");
        assert!(matches!(openrouter, Some((_, Some(_)))));

        // No reading was stored, which is the point: the two facts are separate.
        assert_eq!(history(&connection, "openrouter", 10).expect("history").len(), 0);

        let elsewhere = attempts.iter().find(|(name, _)| name == "cheaperinference");
        assert_eq!(elsewhere, Some(&("cheaperinference".to_string(), None)));
    }

    /// The export is a second way of reading the same rows, so what it must get
    /// right is that a number never arrives without what it means: a spend figure
    /// is not a balance, a 90-day window is not all-time, and two accounts' rows
    /// are not one series. Also pins the order, because a chart built from the
    /// file would be drawn backwards otherwise.
    #[test]
    fn the_export_carries_what_each_number_means() {
        let connection = memory_database();

        // An all-time spend reading for OpenRouter.
        connection
            .execute(
                "INSERT INTO balance_snapshots (provider_id, remaining, basis, usage, recorded_at)
                 SELECT id, 12.5, 'usage', 12.5, '2026-09-24 06:00:00' FROM providers
                 WHERE name = 'openrouter'",
                [],
            )
            .expect("stored");

        // A windowed account balance for CheaperInference, with its own threshold.
        connection
            .execute(
                "INSERT INTO balance_snapshots
                    (provider_id, remaining, basis, account_credits, usage, spend_window_days,
                     provider_threshold_usd, key_fingerprint, recorded_at)
                 SELECT id, 9.5, 'account_credits', 9.5, 4.5, 90, 5.0, 'abc123',
                        '2026-09-20 06:00:00'
                 FROM providers WHERE name = 'cheaperinference'",
                [],
            )
            .expect("stored");

        let csv = history_csv(&connection, None).expect("exported");
        let lines: Vec<&str> = csv.lines().collect();

        assert_eq!(
            lines[0],
            "provider,recorded_at,basis,remaining,account_credits,usage,spend_window_days,\
provider_threshold_usd,key_fingerprint"
        );
        // Two readings, and the older one first.
        assert_eq!(lines.len(), 3);

        let cheaper = lines[1].split(',').collect::<Vec<_>>();
        assert_eq!(cheaper[0], "cheaperinference");
        assert_eq!(cheaper[2], "account_credits");
        assert_eq!(cheaper[7], "5", "the provider's own threshold travels with the row");
        assert_eq!(cheaper[8], "abc123", "so do two accounts' rows stay apart");

        let openrouter = lines[2].split(',').collect::<Vec<_>>();
        assert_eq!(openrouter[0], "openrouter");
        // A spend figure keeps the label saying it is not money left.
        assert_eq!(openrouter[2], "usage");
        // And an all-time figure leaves the window empty rather than saying zero.
        assert_eq!(openrouter[6], "");
        assert_eq!(openrouter[3], "12.5");
    }

    /// A field with a comma in it would shift every later column by one, and a
    /// spreadsheet cannot tell that a column is wrong.
    #[test]
    fn a_field_with_a_comma_in_it_is_quoted() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("two\nlines"), "\"two\nlines\"");
    }

    /// An empty database still exports, with the header and nothing under it.
    #[test]
    fn an_empty_history_exports_just_the_header() {
        let connection = memory_database();

        let csv = history_csv(&connection, None).expect("exported");

        assert_eq!(csv.lines().count(), 1);
        assert!(csv.ends_with('\n'));
    }

    /// The anchor is fixed, so a database can be found before there is a database
    /// to ask. A `location` file inside it is what says the user moved it.
    #[test]
    fn a_moved_data_directory_is_read_back_from_the_anchor() {
        let root = std::env::temp_dir().join("meterix-location-test");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp dir");

        let anchor = root.join(DATA_DIR);
        let moved = root.join("elsewhere");

        // The anchor first, then the anchor after being pointed somewhere else.
        assert_eq!(anchor_directory_in(&root), anchor);

        std::fs::create_dir_all(&anchor).expect("anchor");
        std::fs::write(anchor.join(LOCATION_FILE), moved.display().to_string()).expect("written");
        assert_eq!(data_directory_in(&anchor_directory_in(&root)).expect("resolved"), moved);

        // A relative path is taken from the anchor rather than the process's
        // working directory, which for a packaged app is anywhere at all.
        std::fs::write(anchor.join(LOCATION_FILE), "sibling").expect("written");
        assert_eq!(data_directory_in(&anchor_directory_in(&root)).expect("resolved"), anchor.join("sibling"));

        // An empty file means no opinion, not "the current directory".
        std::fs::write(anchor.join(LOCATION_FILE), "   ").expect("written");
        assert_eq!(data_directory_in(&anchor_directory_in(&root)).expect("resolved"), anchor);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Carrying a database over is the one thing that can quietly lose readings,
    /// so each reason to do nothing is checked rather than assumed.
    #[test]
    fn a_legacy_database_is_carried_over_only_when_there_is_one_to_carry() {
        let root = std::env::temp_dir().join("meterix-adopt-test");
        let _ = std::fs::remove_dir_all(&root);
        let legacy = root.join(LEGACY_DATA_DIR);
        let target = root.join(DATA_DIR);
        std::fs::create_dir_all(&legacy).expect("legacy dir");

        // Nothing at the old location: nothing to do.
        assert_eq!(
            adopt_from(&legacy, &target).expect("checked"),
            None,
            "an absent legacy database is not an error"
        );

        std::fs::write(legacy.join(DB_FILE), b"old readings").expect("legacy db");
        let carried = adopt_from(&legacy, &target).expect("carried");
        assert_eq!(carried, Some(legacy.join(DB_FILE)));
        assert_eq!(
            std::fs::read(target.join(DB_FILE)).expect("copied"),
            b"old readings"
        );
        // Left behind on purpose: it is the only copy of readings that cannot be
        // fetched again.
        assert!(legacy.join(DB_FILE).exists(), "the original is kept");

        // A database already at the target is never overwritten.
        std::fs::write(target.join(DB_FILE), b"newer readings").expect("target db");
        assert_eq!(adopt_from(&legacy, &target).expect("checked"), None);
        assert_eq!(
            std::fs::read(target.join(DB_FILE)).expect("read"),
            b"newer readings"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A write-ahead log not yet checkpointed holds the newest readings, so a copy
    /// of the database alone can be missing them — or missing the schema.
    #[test]
    fn a_legacy_write_ahead_log_is_carried_over_too() {
        let root = std::env::temp_dir().join("meterix-wal-test");
        let _ = std::fs::remove_dir_all(&root);
        let legacy = root.join(LEGACY_DATA_DIR);
        let target = root.join(DATA_DIR);
        std::fs::create_dir_all(&legacy).expect("legacy dir");

        std::fs::write(legacy.join(DB_FILE), b"main").expect("legacy db");
        std::fs::write(legacy.join("meterix.db-wal"), b"uncheckpointed").expect("wal");

        adopt_from(&legacy, &target).expect("carried");

        assert_eq!(
            std::fs::read(target.join("meterix.db-wal")).expect("copied"),
            b"uncheckpointed"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Switching a provider off has to stop it being watched without losing
    /// anything: the key stays, the readings stay, and it comes back whole.
    #[test]
    fn a_provider_that_is_switched_off_stops_being_watched() {
        let connection = memory_database();

        assert_eq!(tracked_providers(&connection).expect("tracked"), every_provider());
        assert_eq!(due_providers(&connection).expect("due").len(), PROVIDERS.len());
        assert_eq!(
            resolved_thresholds(&connection).expect("resolved").len(),
            PROVIDERS.len()
        );

        // A reading, so there is history to keep.
        connection
            .execute(
                "INSERT INTO balance_snapshots (provider_id, remaining, basis, recorded_at)
                 SELECT id, 12.0, 'account_credits', '2026-09-20 06:00:00' FROM providers
                 WHERE name = 'openrouter'",
                [],
            )
            .expect("stored");

        set_provider_enabled(&connection, "openrouter", false).expect("switched off");

        // Not tracked, not fetched, not reported on. Silence is the whole point:
        // a tray that still colours itself for a provider nobody is watching is a
        // lie about what the app knows.
        let tracked_now = tracked_providers(&connection).expect("tracked");
        assert!(!tracked_now.contains(&"openrouter".to_string()), "switched off");
        assert_eq!(tracked_now.len(), PROVIDERS.len() - 1);
        assert_eq!(tracked_now, {
            let mut rest = every_provider();
            rest.retain(|name| name != "openrouter");
            rest
        });

        assert!(!due_providers(&connection)
            .expect("due")
            .iter()
            .any(|name| name == "openrouter"));
        // What an unqualified refresh fetches: asking for everything means
        // everything being watched, not every adapter this build has.
        assert!(!tracked_providers(&connection)
            .expect("tracked")
            .iter()
            .any(|name| name == "openrouter"));
        assert!(
            !resolved_thresholds(&connection)
                .expect("resolved")
                .iter()
                .any(|(name, _)| name == "openrouter"),
            "a switched-off provider cannot recolour the tray"
        );

        // The row, the history and the earliest reading are all still there.
        assert_eq!(history(&connection, "openrouter", 10).expect("history").len(), 1);
        let starts = first_reading_at(&connection).expect("starts");
        assert_eq!(
            starts
                .iter()
                .find(|(name, _)| name == "openrouter")
                .cloned(),
            Some((
                "openrouter".to_string(),
                Some("2026-09-20 06:00:00".to_string())
            ))
        );

        // And switching back on restores it exactly.
        set_provider_enabled(&connection, "openrouter", true).expect("switched on");
        assert_eq!(tracked_providers(&connection).expect("tracked"), every_provider());
        assert_eq!(
            resolved_thresholds(&connection).expect("resolved").len(),
            PROVIDERS.len()
        );

        assert!(set_provider_enabled(&connection, "nowhere", false).is_err());
    }

    /// Every provider a previous build wrote was being polled, so upgrading must
    /// not silently stop watching any of them.
    #[test]
    fn a_provider_from_before_the_column_existed_stays_tracked() {
        let connection = memory_database();

        // The default is what an upgraded row gets: the column is added with one.
        let enabled: i64 = connection
            .query_row(
                "SELECT enabled FROM providers WHERE name = 'openrouter'",
                [],
                |row| row.get(0),
            )
            .expect("enabled");

        assert_eq!(enabled, 1);
        assert_eq!(
            tracked_providers(&connection).expect("tracked").len(),
            PROVIDERS.len()
        );
    }

    /// A character quota is not money, so it must never be compared to a dollar
    /// threshold. Checked against the same numbers as a balance, where the answer
    /// would be "announce it", so the exclusion is doing work rather than the test
    /// passing by accident.
    #[test]
    fn a_quota_is_never_compared_to_a_dollar_threshold() {
        let connection = memory_database();
        let settings = Settings {
            low_balance_threshold: 100.0,
            ..Settings::default()
        };

        let mut quota = balance(Basis::Quota, 5.0, None, None);
        quota.provider = "elevenlabs".to_string();

        let notices = take_notifications(&connection, &[outcome("elevenlabs", Ok(quota))], &settings)
            .expect("notices");
        assert!(notices.is_empty(), "5 characters is not 5 dollars");

        // The same figure as money is below the threshold and is news, which is
        // what makes the line above meaningful.
        let mut money = balance(Basis::AccountCredits, 5.0, Some(5.0), None);
        money.provider = "elevenlabs".to_string();

        let notices = take_notifications(&connection, &[outcome("elevenlabs", Ok(money))], &settings)
            .expect("notices");
        assert_eq!(notices.len(), 1, "$5 under a $100 threshold is announced");
    }

    #[test]
    fn a_quota_survives_a_round_trip_through_the_database() {
        let connection = memory_database();

        let mut reading = balance(Basis::Quota, 4200.0, None, None);
        reading.provider = "elevenlabs".to_string();
        save_snapshot(&connection, &reading).expect("saved");

        let stored = history(&connection, "elevenlabs", 1).expect("history");
        assert_eq!(stored[0].basis, Basis::Quota);
        assert_eq!(stored[0].basis.as_str(), "quota");
        // And it is not money, so it cannot be summed or drawn with money.
        assert!(!stored[0].basis.is_balance());
    }

    /// A fake key for each provider. The five prefixes are not disjoint — an
    /// OpenRouter key also starts with DeepSeek's `sk-` — so the registry order is
    /// load-bearing and a reorder would silently send keys to the wrong adapter.
    #[test]
    fn a_key_prefix_picks_the_right_provider_when_one_prefix_contains_another() {
        assert_eq!(provider_for_key("sk-or-v1-abcdef"), Some("openrouter"));
        assert_eq!(provider_for_key("sk-abcdef"), Some("deepseek"));
        assert_eq!(provider_for_key("sk_abcdef"), Some("elevenlabs"));
        assert_eq!(provider_for_key("ci_live_abcdef"), Some("cheaperinference"));
        assert_eq!(provider_for_key("not-a-key"), None);

        // And the hint names the right provider when one is filed under another.
        let hint = misdirected_key_hint("openrouter", "sk-abcdef").expect("a hint");
        assert!(hint.contains("DeepSeek"), "{hint}");
    }

    /// DeepSeek prices in the account's currency. Only a USD figure can be
    /// compared to a dollar threshold, so anything else has to fail loudly rather
    /// than be relabelled.
    #[test]
    fn a_deepseek_balance_is_only_read_when_it_is_in_dollars() {
        let balance_info = |currency: &str, total: &str| DeepSeekBalance {
            currency: currency.to_string(),
            total_balance: total.to_string(),
        };

        // USD is picked out even when another currency comes first.
        let response = DeepSeekResponse {
            balance_infos: vec![
                balance_info("CNY", "110.00"),
                balance_info("USD", "15.37"),
            ],
        };
        assert_eq!(deepseek_usd_balance(&response).expect("read"), 15.37);

        // A CNY-only account is refused, and the message says which currency it
        // actually found.
        let response = DeepSeekResponse {
            balance_infos: vec![balance_info("CNY", "110.00")],
        };
        let error = deepseek_usd_balance(&response).expect_err("refused");
        assert!(error.to_string().contains("CNY"), "{error}");

        // Nothing at all, and a figure that is not a number.
        assert!(deepseek_usd_balance(&DeepSeekResponse {
            balance_infos: vec![]
        })
        .is_err());
        assert!(deepseek_usd_balance(&DeepSeekResponse {
            balance_infos: vec![balance_info("USD", "not a number")]
        })
        .is_err());
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

    /// A leftover environment variable is the one thing `forget_key` has to
    /// report, because the app cannot unset it for the user. This is the check
    /// behind "Key removed, provider still configured".
    #[test]
    fn an_environment_key_is_what_a_removal_cannot_clear() {
        // This test binary has no other reader of a provider's environment
        // variable, so setting one here cannot disturb another test.
        assert_eq!(env_credential("openrouter"), None);

        // Edition 2024 made mutating the environment unsafe: another thread may
        // be reading it concurrently, which is precisely why the core reads the
        // key once and keeps it rather than looking it up per request.
        unsafe { std::env::set_var("OPENROUTER_KEY", "sk-or-v1-not-a-real-key") };
        assert_eq!(env_credential("openrouter"), Some("OPENROUTER_KEY"));
        unsafe { std::env::remove_var("OPENROUTER_KEY") };

        assert_eq!(env_credential("openrouter"), None);
    }

    /// The provider is checked before any keychain work, so a typo on the
    /// command line cannot reach the credential store at all.
    #[test]
    fn forgetting_an_unknown_provider_touches_no_credential() {
        assert!(forget_key("not-a-provider").is_err());
    }

    /// When a provider's history starts comes from its earliest reading, so it
    /// works for a provider that was configured before the app first ran.
    #[test]
    fn a_history_start_is_the_earliest_reading_not_the_newest() {
        let connection = memory_database();

        // Nothing stored yet: no history to date.
        let starts = first_reading_at(&connection).expect("starts");
        assert_eq!(starts.iter().find(|(name, _)| name == "openrouter"), Some(&("openrouter".to_string(), None)));

        for (remaining, recorded_at) in [(12.0, "2026-09-20 06:00:00"), (4.0, "2026-09-24 22:00:00")] {
            connection
                .execute(
                    "INSERT INTO balance_snapshots (provider_id, remaining, basis, recorded_at)
                     SELECT id, ?1, 'account_credits', ?2 FROM providers WHERE name = 'openrouter'",
                    params![remaining, recorded_at],
                )
                .expect("stored");
        }

        let starts = first_reading_at(&connection).expect("starts");
        assert_eq!(
            starts.iter().find(|(name, _)| name == "openrouter"),
            Some(&("openrouter".to_string(), Some("2026-09-20 06:00:00".to_string())))
        );
        // A provider with no readings says so rather than claiming today.
        assert_eq!(
            starts.iter().find(|(name, _)| name == "cheaperinference"),
            Some(&("cheaperinference".to_string(), None))
        );
    }
}
