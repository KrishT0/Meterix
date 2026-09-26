import { invoke } from '@tauri-apps/api/core'

/** Where a stored number came from. Mirrors `meterix_core::Basis`. */
export type Basis = 'account_credits' | 'key_cap' | 'usage'

/** Identifiers from `meterix_core::ProviderError::kind()`. */
export type ErrorKind =
  | 'missing_credential'
  | 'unauthorized'
  | 'forbidden'
  | 'rate_limited'
  | 'unreachable'
  | 'bad_response'

export interface ProviderOverview {
  name: string
  displayName: string
  configured: boolean
  keyHint: string | null
  balance: number | null
  basis: Basis | null
  accountCredits: number | null
  usage: number | null
  spendWindowDays: number | null
  recordedAt: string | null
  /** Already resolved from this provider's override or the app default. */
  threshold: number
  /** Labels the credential in use right now. Never the key itself. */
  keyFingerprint: string | null
}

export interface RefreshOutcome {
  provider: string
  displayName: string
  ok: boolean
  balance: number | null
  basis: Basis | null
  accountCredits: number | null
  usage: number | null
  spendWindowDays: number | null
  errorKind: ErrorKind | null
  errorMessage: string | null
}

export interface SnapshotRow {
  recordedAt: string
  basis: Basis
  accountCredits: number | null
  usage: number | null
  spendWindowDays: number | null
  remaining: number
  /** Labels the credential that produced this reading. Null on older rows. */
  keyFingerprint: string | null
}

/** Everything the settings screen needs. */
export interface SettingsView {
  pollIntervalMinutes: number
  lowBalanceThreshold: number
  notifyLowBalance: boolean
  notifyKeyErrors: boolean
  /** The file the database is read from right now. */
  databasePath: string
  /**
   * The folder the database will be read from on the next launch. Differs from
   * the folder above only between choosing a new one and restarting.
   */
  dataDirectory: string
  /** Where CSV exports are written, already resolved. */
  exportDirectory: string
  autostartEnabled: boolean
  providers: ProviderSetting[]
}

export interface ProviderSetting {
  /**
   * Whether the app is watching this provider. Off keeps the key and the
   * readings and stops the polling.
   */
  enabled: boolean
  name: string
  displayName: string
  /** `null` when this provider has no threshold of its own. */
  lowBalanceThreshold: number | null
  /**
   * What applies when this provider's box is left blank: the provider's own
   * published threshold where it has one, otherwise the app default. Shown as
   * the placeholder, so a blank box cannot mean something different here than
   * it does at poll time.
   */
  fallbackThreshold: number
  /**
   * The earliest reading stored for this provider, so "since" is a fact rather
   * than a note taken when the row was first written. `null` until it has one.
   */
  firstReadingAt: string | null
  /** `null` when this provider uses the app-wide interval. */
  pollIntervalMinutes: number | null
  keyHint: string
}

/** The app-wide settings and the autostart preference, saved as one form. */
export interface SettingsInput {
  pollIntervalMinutes: number
  lowBalanceThreshold: number
  notifyLowBalance: boolean
  notifyKeyErrors: boolean
}

export const settings = () => invoke<SettingsView>('settings')

export const saveSettings = (settings: SettingsInput, autostart: boolean) =>
  invoke<void>('save_settings', { settings, autostart })

/** `null` clears the override and puts the provider back on the app default. */
export const setProviderThreshold = (provider: string, threshold: number | null) =>
  invoke<void>('set_provider_threshold', { provider, threshold })

/** `null` puts the provider back on the app-wide interval. */
export const setProviderInterval = (provider: string, minutes: number | null) =>
  invoke<void>('set_provider_interval', { provider, minutes })

/**
 * Watch this provider, or stop watching it.
 *
 * Nothing is deleted either way: the key stays in the keychain and the readings
 * stay in the database. A provider switched off is not polled and not reported
 * on, which is why the tray is recoloured by the same call.
 */
export const setProviderEnabled = (provider: string, enabled: boolean) =>
  invoke<void>('set_provider_enabled', { provider, enabled })

/**
 * Write the reading history out as CSV, in the chosen folder.
 *
 * Resolves with the path written. The file is derived from the database, so
 * calling this again replaces it rather than leaving copies to accumulate.
 */
export const exportHistory = (provider?: string) =>
  invoke<string>('export_history', { provider: provider ?? null })

/**
 * Choose a different folder for the database. Takes effect on the next launch.
 *
 * Resolves with the chosen folder, or `null` if the picker was cancelled.
 */
export const setDataDirectory = () => invoke<string | null>('set_data_directory')

/** Choose where CSV exports are written. `null` if the picker was cancelled. */
export const setExportDirectory = () => invoke<string | null>('set_export_directory')

/** What happened when a key was offered for saving. */
export interface SaveKeyOutcome {
  provider: string
  displayName: string
  /**
   * `saved_verified` — the key works and a balance was read.
   * `saved_unverified` — saved, but no balance could be read.
   * `rejected` — the provider refused it, and nothing was written.
   */
  status: 'saved_verified' | 'saved_unverified' | 'rejected'
  balance: number | null
  errorMessage: string | null
}

/** Supported providers, and the latest stored reading for each. */
export const overview = () => invoke<ProviderOverview[]>('overview')

/** Fetch from the providers and store what came back. One entry per provider. */
export const refresh = (only?: string) =>
  invoke<RefreshOutcome[]>('refresh', { only: only ?? null })

export const snapshotHistory = (provider: string, limit: number) =>
  invoke<SnapshotRow[]>('snapshot_history', { provider, limit })

/**
 * Verify a key and, only if it works, replace the stored one.
 *
 * Resolves with an outcome rather than throwing: a rejected key is an expected
 * result, not a failure of the call.
 */
export const setKey = (provider: string, key: string) =>
  invoke<SaveKeyOutcome>('set_key', { provider, key })

/**
 * Remove a stored key.
 *
 * Resolves with the environment variable still supplying a key, if one is. The
 * keychain entry is the app's to delete; the caller's environment is not, so a
 * provider can stay configured after its key was removed, and the window needs
 * to be able to say so.
 */
export const removeProvider = (provider: string) =>
  invoke<string | null>('remove_provider', { provider })
