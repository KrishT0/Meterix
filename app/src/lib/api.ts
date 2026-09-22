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
  configured: boolean
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

/** Supported providers, and the latest stored reading for each. */
export const overview = () => invoke<ProviderOverview[]>('overview')

/** Fetch from the providers and store what came back. One entry per provider. */
export const refresh = (only?: string) =>
  invoke<RefreshOutcome[]>('refresh', { only: only ?? null })

export const snapshotHistory = (provider: string, limit: number) =>
  invoke<SnapshotRow[]>('snapshot_history', { provider, limit })

export const setKey = (provider: string, key: string) =>
  invoke<void>('set_key', { provider, key })

export const removeProvider = (provider: string) =>
  invoke<void>('remove_provider', { provider })
