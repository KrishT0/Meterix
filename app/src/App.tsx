import { useCallback, useEffect, useRef, useState } from 'react'

import { BalanceChart, type Series } from './components/BalanceChart'
import { ProviderCard } from './components/ProviderCard'
import { ProviderPicker } from './components/ProviderPicker'
import { ProviderTable } from './components/ProviderTable'
import {
  Button,
  Label,
  LOW_BALANCE_THRESHOLD,
  Pill,
  RefreshIcon,
  StatusDot,
  healthOf,
  tones,
} from './components/ui'
import type { ProviderOverview, RefreshOutcome, SnapshotRow } from './lib/api'
import * as api from './lib/api'
import { amount, daySeconds, parseUtc, relativeTime, usd } from './lib/format'

/** Enough for a month of half-hourly polls, which is what the chart wants. */
const HISTORY_LIMIT = 1500

/**
 * Spend per day, from the drop between the first and last stored balance for
 * each provider. Null until some provider has two dated readings, because
 * guessing from one point would be inventing a number.
 */
function burnPerDay(readings: Record<string, SnapshotRow[]>): number | null {
  let spent = 0
  let seconds = 0

  for (const rows of Object.values(readings)) {
    const balances = rows
      .filter((row) => row.basis !== 'usage')
      .slice()
      .sort((a, b) => parseUtc(a.recordedAt).getTime() - parseUtc(b.recordedAt).getTime())

    const first = balances.at(0)
    const last = balances.at(-1)
    if (!first || !last || first === last) continue

    const drop = first.remaining - last.remaining
    const elapsed = daySeconds(last.recordedAt) - daySeconds(first.recordedAt)
    if (drop <= 0 || elapsed <= 0) continue

    spent += drop
    seconds += elapsed
  }

  if (seconds <= 0) return null
  return (spent / seconds) * 86_400
}

export default function App() {
  const [overview, setOverview] = useState<ProviderOverview[]>([])
  const [outcomes, setOutcomes] = useState<Record<string, RefreshOutcome>>({})
  const [readings, setReadings] = useState<Record<string, SnapshotRow[]>>({})
  const [fatal, setFatal] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [saving, setSaving] = useState(false)
  const [ready, setReady] = useState(false)
  const [keyDraft, setKeyDraft] = useState('')
  const [chosen, setChosen] = useState<string | null>(null)

  const started = useRef(false)

  const load = useCallback(async () => {
    const rows = await api.overview()
    setOverview(rows)

    const pairs = await Promise.all(
      rows.map(async (row) => [row.name, await api.snapshotHistory(row.name, HISTORY_LIMIT)] as const),
    )
    setReadings(Object.fromEntries(pairs))

    return rows
  }, [])

  const refreshAll = useCallback(async () => {
    setBusy(true)
    try {
      const results = await api.refresh()
      setOutcomes(Object.fromEntries(results.map((result) => [result.provider, result])))
      await load()
      setFatal(null)
    } catch (error) {
      setFatal(String(error))
    } finally {
      setBusy(false)
      setReady(true)
    }
  }, [load])

  useEffect(() => {
    // StrictMode runs effects twice in development. Without this guard every
    // window open would spend two rounds of API calls and store two of
    // everything.
    if (started.current) return
    started.current = true

    void (async () => {
      try {
        const rows = await load()
        setReady(true)
        // Only reach out if there is actually a key to use.
        if (rows.some((row) => row.configured)) await refreshAll()
      } catch (error) {
        setFatal(String(error))
        setReady(true)
      }
    })()
  }, [load, refreshAll])

  const supported = overview.map((row) => row.name)
  const active = chosen ?? supported[0] ?? ''
  const configured = overview.filter((row) => row.configured)
  // Only providers that still hold a key get a card or a table row. Removing a
  // provider has to actually remove it from the dashboard, and the key row at
  // the top is where it comes back from.
  const shown = configured

  const balances = overview.filter((row) => row.balance !== null && row.basis !== 'usage')
  const totalBalance = balances.reduce((sum, row) => sum + (row.balance ?? 0), 0)

  const priced = overview.filter((row) => row.accountCredits !== null)
  // Only all-time spend is summed. A windowed figure folded into the same total
  // would be adding unlike numbers, so it is left to the table.
  const allTimeSpend = overview.filter(
    (row) => row.usage !== null && row.spendWindowDays === null,
  )
  const totalSpend = allTimeSpend.reduce((sum, row) => sum + (row.usage ?? 0), 0)
  const totalPurchased = priced
    .filter((row) => row.usage !== null && row.spendWindowDays === null)
    .reduce((sum, row) => sum + (row.accountCredits ?? 0) + (row.usage ?? 0), 0)

  const burn = burnPerDay(readings)
  const lastReading = overview
    .map((row) => row.recordedAt)
    .filter((value): value is string => value !== null)
    .sort()
    .at(-1)

  const healths = overview.map((row) => healthOf(row, outcomes[row.name]))
  // Teal means "checked and fine". With nothing configured, or nothing read
  // yet, there is no such claim to make, so the pill stays neutral.
  const overallTone = healths.includes('error')
    ? 'copper'
    : healths.includes('low')
      ? 'amber'
      : healths.includes('ok')
        ? 'teal'
        : 'muted'
  const problems = overview.filter((row) => {
    const health = healthOf(row, outcomes[row.name])
    return row.configured && (health === 'low' || health === 'error')
  })

  // Provider failures need somewhere to show even when nothing is configured.
  // Otherwise a key that never got saved leaves the empty state sitting there
  // saying nothing at all, which reads as a dead button.
  const failed = Object.values(outcomes).filter((outcome) => {
    if (outcome.ok) return false
    // "no key set" is not worth shouting about before the user has added one.
    const isConfigured = overview.find((row) => row.name === outcome.provider)?.configured
    return !(outcome.errorKind === 'missing_credential' && isConfigured !== true)
  })

  const selected = overview.find((row) => row.name === active)
  const series: Series[] = shown.map((row, index) => ({
    name: row.name,
    tone: index === 0 ? 'teal' : 'copper',
    points: readings[row.name] ?? [],
  }))

  async function addProvider() {
    const trimmed = keyDraft.trim()
    if (!trimmed || busy || !active) return

    setSaving(true)
    setBusy(true)
    try {
      await api.setKey(active, trimmed)
      setKeyDraft('')
      setFatal(null)
      await refreshAll()
    } catch (error) {
      setFatal(String(error))
    } finally {
      setSaving(false)
      setBusy(false)
    }
  }

  async function removeProvider(name: string) {
    const confirmed = window.confirm(
      `Remove the API key for ${name}?\n\nStored readings are kept, so adding the key back later keeps its history.`,
    )
    if (!confirmed) return

    setBusy(true)
    try {
      await api.removeProvider(name)
      setOutcomes((prev) =>
        Object.fromEntries(Object.entries(prev).filter(([key]) => key !== name)),
      )
      await load()
    } catch (error) {
      setFatal(String(error))
    } finally {
      setBusy(false)
    }
  }

  return (
    // The window is the app. No backdrop strip, no rounded panel sitting inside
    // a page, so the content meets the window edges directly.
    <div className="flex min-h-full flex-col bg-surface">
      <header className="flex items-center gap-3 border-b border-line px-6 py-4">
        <span className="text-[15px] font-semibold tracking-[-0.01em]">Meterix</span>

        <Pill className={`flex items-center gap-1.5 ${tones[overallTone].pill}`}>
          <StatusDot tone={overallTone} />
          {configured.length} tracked
        </Pill>

        <div className="ml-auto flex items-center gap-3">
          <span className="num text-[12px] text-ink-muted">
            updated {relativeTime(lastReading ?? null)}
          </span>
          <Button variant="secondary" onClick={() => void refreshAll()} disabled={busy}>
            <span className="flex items-center gap-2">
              <RefreshIcon className={busy ? 'animate-spin' : ''} />
              {busy ? 'Refreshing' : 'Refresh'}
            </span>
          </Button>
        </div>
      </header>

      <div className="flex-1 space-y-6 p-6">
        {fatal ? (
          <div className="rounded-[10px] border border-copper-dim/40 bg-copper-tint px-4 py-3">
            <span className="label-sm text-copper">Could not reach the core</span>
            <p className="num mt-1 text-[12px] text-ink-dim">{fatal}</p>
          </div>
        ) : null}

        {failed.length > 0 ? (
          <div className="rounded-[10px] border border-copper-dim/40 bg-copper-tint px-4 py-3">
            <span className="label-sm text-copper">
              Could not read {failed.length === 1 ? 'a provider' : 'some providers'}
            </span>
            <ul className="mt-2 space-y-1">
              {failed.map((outcome) => (
                <li key={outcome.provider} className="num text-[12px] text-ink-dim">
                  {outcome.displayName}: {outcome.errorMessage}
                </li>
              ))}
            </ul>
          </div>
        ) : null}

        <div className="flex items-start gap-10">
          <div>
            <Label className="text-ink-muted">Total balance</Label>
            <div className="mt-2 flex items-baseline gap-2">
              <span className="num text-[46px] leading-none font-bold tracking-[-0.03em]">
                {balances.length === 0 ? '—' : amount(totalBalance)}
              </span>
              {balances.length === 0 ? null : (
                <span className="num text-[13px] text-ink-muted">USD</span>
              )}
            </div>
            <div className="mt-3 flex flex-wrap items-center gap-x-5 gap-y-1">
              <span className="label-sm text-ink-muted">
                Spent all time{' '}
                <span className="num text-ink-dim">
                  {allTimeSpend.length === 0 ? '—' : usd(totalSpend).slice(1)}
                </span>
              </span>
              {totalPurchased > 0 ? (
                <span className="label-sm text-ink-muted">
                  Top-ups{' '}
                  <span className="num text-ink-dim">{usd(totalPurchased).slice(1)}</span>
                </span>
              ) : null}
              <span className="label-sm text-ink-muted">
                Burn{' '}
                <span className="num text-ink-dim">
                  {burn === null ? '—' : `${burn.toFixed(2)}/day`}
                </span>
              </span>
            </div>
          </div>

          {problems.length > 0 ? (
            <div className="mt-7 flex items-center gap-2 rounded-[10px] border border-amber-line bg-amber-tint px-3.5 py-2.5">
              <span className="num text-[12px] text-amber-dim">
                {problems.length} provider{problems.length === 1 ? '' : 's'} need attention
              </span>
            </div>
          ) : null}

          <div className="num ml-auto mt-7 text-right text-[12px] text-ink-muted">
            <div className="label-sm">Providers</div>
            <div className="mt-1.5 text-ink-dim">
              {configured.length} of {supported.length} configured
            </div>
            <div className="label-sm mt-1.5">
              low below {usd(LOW_BALANCE_THRESHOLD).slice(1)}
            </div>
          </div>
        </div>

        {ready && supported.length > 0 ? (
          <div className="flex flex-wrap items-stretch gap-2.5">
            <div className="flex min-w-[320px] flex-1 items-center gap-3 rounded-[10px] border border-line bg-inset px-4 py-3">
              <Label className="text-ink-muted">Key</Label>
              <input
                type="password"
                value={keyDraft}
                onChange={(event) => setKeyDraft(event.target.value)}
                onKeyDown={(event) => {
                  if (event.key === 'Enter') void addProvider()
                }}
                placeholder="paste an api key"
                className="num min-w-0 flex-1 bg-transparent text-[13px] text-ink outline-none placeholder:text-ink-muted"
              />
              <ProviderPicker providers={overview} value={active} onChange={setChosen} />
            </div>
            <Button
              variant="primary"
              onClick={() => void addProvider()}
              disabled={busy || keyDraft.trim() === ''}
              className="shrink-0 px-6"
            >
              {saving ? 'Saving…' : 'Save key'}
            </Button>
            <div className="num w-full pl-1 text-[11px] text-ink-muted">
              {selected?.keyHint ? (
                <>
                  stored for {selected.displayName}:{' '}
                  <span className="text-ink-dim">{selected.keyHint}</span>
                </>
              ) : (
                <>no key stored for {selected?.displayName ?? active}</>
              )}
            </div>
          </div>
        ) : null}

        {ready && configured.length === 0 ? (
          <div className="flex flex-col items-center rounded-[12px] border border-line bg-inset px-8 py-14 text-center">
            <h2 className="text-[16px] font-medium">No providers connected</h2>
            <p className="num mt-2 max-w-[380px] text-[12px] leading-relaxed text-ink-muted">
              Paste an API key above to start tracking a balance. Keys go to your OS keychain
              and are never written to the database.
            </p>
          </div>
        ) : null}

        {configured.length > 0 ? (
          <>
            <div className="flex items-center justify-between pt-1">
              <Label className="text-ink-muted">Providers</Label>
              <span className="num text-[11px] text-ink-muted">
                {configured.length} configured · {readingsCount(readings)} readings stored
              </span>
            </div>

            <div className="grid grid-cols-1 gap-2.5 xl:grid-cols-2">
              {shown.map((row) => (
                <ProviderCard
                  key={row.name}
                  provider={row}
                  outcome={outcomes[row.name]}
                  readings={readings[row.name] ?? []}
                  onRemove={() => void removeProvider(row.name)}
                  busy={busy}
                />
              ))}
            </div>

            <BalanceChart series={series} />

            <ProviderTable providers={shown} outcomes={outcomes} />

            <div className="flex items-start gap-2.5 rounded-[10px] border border-line bg-inset px-4 py-3">
              <p className="num text-[12px] leading-relaxed text-ink-muted">
                Not every key reports a balance. Rows stored with{' '}
                <span className="text-copper">basis = usage</span> hold spend instead, so they
                are kept out of the chart. A rising spend line would read like a healthy
                balance.
              </p>
            </div>
          </>
        ) : null}
      </div>

      {configured.length > 0 ? (
        <footer className="flex items-center gap-3 border-t border-line px-6 py-3.5">
          <span className="num text-[12px] text-ink-muted">
            last reading {relativeTime(lastReading ?? null)} · {readingsCount(readings)} snapshots
            in the database
          </span>
        </footer>
      ) : null}
    </div>
  )
}

function readingsCount(readings: Record<string, SnapshotRow[]>): number {
  return Object.values(readings).reduce((total, rows) => total + rows.length, 0)
}
