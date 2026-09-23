import { useCallback, useEffect, useRef, useState } from 'react'
import { listen } from '@tauri-apps/api/event'

import { BalanceChart, type Series } from './components/BalanceChart'
import { ProviderCard } from './components/ProviderCard'
import { ProviderPicker } from './components/ProviderPicker'
import { ProviderTable } from './components/ProviderTable'
import { Settings } from './Settings'
import {
  Button,
  Label,
  Pill,
  RefreshIcon,
  StatusDot,
  healthOf,
  tones,
} from './components/ui'
import type { ProviderOverview, RefreshOutcome, SaveKeyOutcome, SnapshotRow } from './lib/api'
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
  const [saveResult, setSaveResult] = useState<SaveKeyOutcome | null>(null)
  const [view, setView] = useState<'dashboard' | 'settings'>('dashboard')
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

  useEffect(() => {
    // The poller runs on its own timer, and the tray menu can refresh too.
    // Either way the database is the source of truth, so just read it again.
    const stop = listen('balances-updated', () => {
      void load().catch((error) => setFatal(String(error)))
    })

    return () => {
      void stop.then((unlisten) => unlisten())
    }
  }, [load])

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
      const result = await api.setKey(active, trimmed)
      setSaveResult(result)

      if (result.status === 'rejected') {
        // Nothing was written, so keep the draft for correcting rather than
        // making it be retyped.
        return
      }

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
      {view === 'settings' ? (
        <>
          {/* Breadcrumb rather than reusing the dashboard header, so leaving is
              at the top instead of below every setting. */}
          <div className="flex items-center gap-2.5 border-b border-line px-5 py-3">
            <button
              type="button"
              onClick={() => setView('dashboard')}
              className="flex items-center gap-1.5 text-ink-dim transition hover:text-ink"
            >
              <svg
                width="11"
                height="11"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2.4"
                strokeLinecap="round"
                strokeLinejoin="round"
                aria-hidden="true"
              >
                <path d="m15 18-6-6 6-6" />
              </svg>
              <span className="text-[12px]">Dashboard</span>
            </button>
            <span className="text-line-strong">/</span>
            <span className="text-[13px] font-semibold tracking-[-0.01em]">Settings</span>
          </div>

          <Settings onClose={() => setView('dashboard')} />
        </>
      ) : (
        <>
      <header className="flex items-center gap-2.5 border-b border-line px-5 py-3">
        <span className="text-[13px] font-semibold tracking-[-0.01em]">Meterix</span>

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
          {/* Labelled rather than a bare gear: there is only one other control
              up here, and an unlabelled icon would be guesswork. */}
          <Button variant="secondary" onClick={() => setView('settings')}>
            <span className="flex items-center gap-2">
              <svg
                width="12"
                height="12"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="1.9"
                strokeLinecap="round"
                strokeLinejoin="round"
                aria-hidden="true"
              >
                <circle cx="12" cy="12" r="3" />
                <path d="M19.4 15a1.7 1.7 0 0 0 .3 1.9l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.9-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1A1.7 1.7 0 0 0 9 19.4a1.7 1.7 0 0 0-1.9.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.9 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1A1.7 1.7 0 0 0 4.6 9a1.7 1.7 0 0 0-.3-1.9l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.9.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.9-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.9V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1Z" />
              </svg>
              Settings
            </span>
          </Button>
        </div>
      </header>

      <div className="flex-1 p-4">
        {fatal || failed.length > 0 ? (
          <div className="space-y-4">
            {fatal ? (
              <div className="rounded-lg border border-copper-dim/40 bg-copper-tint px-3.5 py-2.5">
                <span className="label-sm text-copper">Could not reach the core</span>
                <p className="num mt-1 text-[11px] text-ink-dim">{fatal}</p>
              </div>
            ) : null}

            {failed.length > 0 ? (
              <div className="rounded-lg border border-copper-dim/40 bg-copper-tint px-3.5 py-2.5">
                <span className="label-sm text-copper">
                  Could not read {failed.length === 1 ? 'a provider' : 'some providers'}
                </span>
                <ul className="mt-1.5 space-y-1">
                  {failed.map((outcome) => (
                    <li key={outcome.provider} className="num text-[11px] text-ink-dim">
                      {outcome.displayName}: {outcome.errorMessage}
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}
          </div>
        ) : null}

        {/*
          Zones are separated by 40px and the items inside one by 16px. Uniform
          spacing made the summary, the cards, the chart and the table read as a
          single block.
        */}
        <div className={`space-y-4 ${fatal || failed.length > 0 ? 'mt-10' : ''}`}>
          <div className="flex items-start gap-8">
            <div>
              <Label className="text-ink-muted">Total balance</Label>
              <div className="mt-1.5 flex items-baseline gap-2">
                <span className="num text-[34px] leading-none font-bold tracking-[-0.03em]">
                  {balances.length === 0 ? '—' : amount(totalBalance)}
                </span>
                {balances.length === 0 ? null : (
                  <span className="num text-[12px] text-ink-muted">USD</span>
                )}
              </div>
              <div className="mt-2.5 flex flex-wrap items-center gap-x-4 gap-y-1">
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
              <div className="mt-5 flex items-center gap-2 rounded-lg border border-amber-line bg-amber-tint px-3 py-2">
                <span className="num text-[12px] text-amber-dim">
                  {problems.length} provider{problems.length === 1 ? '' : 's'}{' '}
                  {problems.length === 1 ? 'needs' : 'need'} attention
                </span>
              </div>
            ) : null}

            <div className="num ml-auto mt-5 text-right text-[11px] text-ink-muted">
              <div className="label-sm">Providers</div>
              <div className="mt-1 text-ink-dim">
                {configured.length} of {supported.length} configured
              </div>
              <div className="label-sm mt-1">
                {/* One number only when every provider agrees on one. */}
                {shown.length === 0
                  ? '—'
                  : shown.every((row) => row.threshold === shown[0]?.threshold)
                    ? `low below ${usd(shown[0]?.threshold ?? null).slice(1)}`
                    : 'thresholds per provider'}
              </div>
            </div>
          </div>

          {ready && supported.length > 0 ? (
            <div>
              <div className="flex flex-wrap items-stretch gap-2">
                <div className="flex min-w-[300px] flex-1 items-center gap-2.5 rounded-lg border border-line bg-inset px-3 py-2">
                  <Label className="text-ink-muted">Key</Label>
                  <input
                    type="password"
                    value={keyDraft}
                    onChange={(event) => {
                      setKeyDraft(event.target.value)
                      // A message about the previous attempt is stale the moment
                      // the field changes.
                      setSaveResult(null)
                    }}
                    onKeyDown={(event) => {
                      if (event.key === 'Enter') void addProvider()
                    }}
                    placeholder="paste an api key"
                    className="num min-w-0 flex-1 bg-transparent text-[12px] text-ink outline-none placeholder:text-ink-muted"
                  />
                  <ProviderPicker providers={overview} value={active} onChange={setChosen} />
                </div>
                <Button
                  variant="primary"
                  onClick={() => void addProvider()}
                  disabled={busy || keyDraft.trim() === ''}
                  className="shrink-0 px-4"
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

                {saveResult ? (
                  <div
                    className={`num w-full pl-1 text-[11px] ${
                      saveResult.status === 'rejected'
                        ? 'text-copper'
                        : saveResult.status === 'saved_verified'
                          ? 'text-teal'
                          : 'text-amber-dim'
                    }`}
                  >
                    {saveResult.status === 'rejected' ? (
                      <>
                        not saved · {saveResult.displayName} refused this key.{' '}
                        {saveResult.errorMessage} The stored key is unchanged.
                      </>
                    ) : saveResult.status === 'saved_verified' ? (
                      <>
                        saved · {saveResult.displayName} reports {usd(saveResult.balance)}
                      </>
                    ) : (
                      <>
                        saved, but no balance could be read · {saveResult.errorMessage}
                      </>
                    )}
                  </div>
                ) : null}
              </div>
            </div>
          ) : null}
        </div>

        {ready && configured.length === 0 ? (
          <div className="mt-10 flex flex-col items-center rounded-[10px] border border-line bg-inset px-6 py-9 text-center">
            <h2 className="text-[14px] font-medium">No providers connected</h2>
            <p className="num mt-1.5 max-w-[420px] text-[11px] leading-relaxed text-ink-muted">
              Paste a key above and choose the provider it belongs to. Keys go to your OS keychain
              and are never written to the database.
            </p>
          </div>
        ) : null}

        {configured.length > 0 ? (
          <>
            {/* zone: providers */}
            <div className="mt-10">
              <div className="flex items-center justify-between">
                <Label className="text-ink-muted">Providers</Label>
                <span className="num text-[11px] text-ink-muted">
                  {configured.length} configured · {readingsCount(readings)} readings stored
                </span>
              </div>

              {/* lg (1024px) rather than xl (1280px): the window is 1080 wide, so
                  the two-column layout would never have been reached. */}
              <div className="mt-3 grid grid-cols-1 gap-2 lg:grid-cols-2">
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
            </div>

            {/* zone: chart */}
            <div className="mt-10">
              <BalanceChart series={series} />
            </div>

            {/* zone: table */}
            <div className="mt-10">
              <ProviderTable providers={shown} outcomes={outcomes} />
            </div>

            {/* zone: note */}
            <div className="mt-10 flex items-start gap-2.5 rounded-[10px] border border-line bg-inset px-3.5 py-2.5">
              <p className="num text-[11px] leading-relaxed text-ink-muted">
                Not every key reports a balance. Rows stored with{' '}
                <span className="text-copper">basis = usage</span> hold spend instead, so they
                are kept out of the chart. A rising spend line would read like a healthy
                balance.
              </p>
            </div>
          </>
        ) : null}
      </div>
        </>
      )}

      {view === 'dashboard' && configured.length > 0 ? (
        <footer className="flex items-center gap-3 border-t border-line px-5 py-2.5">
          <span className="num text-[11px] text-ink-muted">
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
