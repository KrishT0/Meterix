import type { Basis, ErrorKind } from './api'

/**
 * SQLite's CURRENT_TIMESTAMP is UTC in "YYYY-MM-DD HH:MM:SS" form. Left to
 * itself JavaScript would read that as local time, which silently shifts every
 * timestamp by the UTC offset, so the Z is added explicitly.
 */
export function parseUtc(value: string): Date {
  return new Date(`${value.replace(' ', 'T')}Z`)
}

/** "$6.40", or an em dash when there is nothing to show. */
export function usd(value: number | null | undefined): string {
  if (value === null || value === undefined) return '—'
  return `$${value.toFixed(2)}`
}

/** Bare number for places that print the currency separately. */
export function amount(value: number | null | undefined): string {
  if (value === null || value === undefined) return '—'
  return value.toFixed(2)
}

export function relativeTime(value: string | null): string {
  if (!value) return 'never'

  const seconds = (Date.now() - parseUtc(value).getTime()) / 1000
  if (!Number.isFinite(seconds)) return 'never'
  if (seconds < 45) return 'just now'
  if (seconds < 3600) return `${Math.round(seconds / 60)}m ago`
  if (seconds < 86400) return `${Math.round(seconds / 3600)}h ago`
  return `${Math.round(seconds / 86400)}d ago`
}

export function localTime(value: string | null): string {
  if (!value) return 'never checked'
  return parseUtc(value).toLocaleString()
}

/** "23 Sep" for a date shown beside other per-provider facts, or an em dash. */
export function shortDate(value: string | null | undefined): string {
  if (!value) return '\u2014'
  return parseUtc(value).toLocaleDateString(undefined, { day: 'numeric', month: 'short' })
}

export function daySeconds(value: string | null): number {
  if (!value) return 0
  return parseUtc(value).getTime() / 1000
}

export function basisLabel(basis: Basis | null): string {
  switch (basis) {
    case 'account_credits':
      return 'Account balance'
    case 'key_cap':
      return 'Key cap remaining'
    case 'usage':
      return 'Spend so far, not a balance'
    case null:
      return 'No reading yet'
  }
}

export function errorHeadline(kind: ErrorKind | null | undefined): string {
  switch (kind) {
    case 'unauthorized':
      return 'Key rejected'
    case 'forbidden':
      return 'No account access'
    case 'rate_limited':
      return 'Rate limited'
    case 'unreachable':
      return 'No connection'
    case 'missing_credential':
      return 'No key set'
    case 'bad_response':
      return 'Unexpected reply'
    default:
      return 'Failed'
  }
}
