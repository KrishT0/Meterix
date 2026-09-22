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
  /** The id, as used in the database and on the command line. */
  name: string
  /** How the provider is written for a person. */
  displayName: string
  configured: boolean
  /** The stored key's format prefix and last four characters, if there is one. */
  keyHint: string | null
  balance: number | null
  basis: Basis | null
  accountCredits: number | null
  usage: number | null
  /** Days the usage figure covers. `null` means all-time. */
  spendWindowDays: number | null
  recordedAt: string | null
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
}

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

export const removeProvider = (provider: string) =>
  invoke<void>('remove_provider', { provider })
