import { useCallback, useEffect, useState } from 'react'
import type { CSSProperties } from 'react'
import { listen } from '@tauri-apps/api/event'
import { invoke } from '@tauri-apps/api/core'

import { Button, WindowControls, healthOf, tones } from './components/ui'
import type { ProviderOverview, RefreshOutcome } from './lib/api'
import * as api from './lib/api'
import { relativeTime, usd } from './lib/format'
import { hueAt } from './lib/palette'

/**
 * The tray popover: one row per provider, and nothing else.
 *
 * It reads from the database rather than fetching on mount, because the
 * dashboard already refreshes at startup and a second fetch from a window
 * nobody is looking at would spend a request for nothing.
 */
export function TrayPopover() {
  const [overview, setOverview] = useState<ProviderOverview[]>([])
  const [outcomes, setOutcomes] = useState<Record<string, RefreshOutcome>>({})
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      setOverview(await api.overview())
      setError(null)
    } catch (problem) {
      setError(String(problem))
    }
  }, [])

  const refresh = useCallback(async () => {
    setBusy(true)
    try {
      const results = await api.refresh()
      setOutcomes(Object.fromEntries(results.map((result) => [result.provider, result])))
      await load()
    } catch (problem) {
      setError(String(problem))
    } finally {
      setBusy(false)
    }
  }, [load])

  useEffect(() => {
    void load()

    // Anything that refreshes — the poller, the tray menu, the dashboard —
    // announces it, so this cannot go stale behind the dashboard's back.
    const stop = listen('balances-updated', () => void load())

    // The popover is hidden whenever it loses focus, so it is only ever visible
    // just after being focused. Re-reading here means the numbers are right when
    // they appear, rather than whatever the last poll stored, which could be a
    // whole interval old with nothing on screen admitting it.
    const onFocus = () => void load()
    window.addEventListener('focus', onFocus)

    return () => {
      window.removeEventListener('focus', onFocus)
      void stop.then((unlisten) => unlisten())
    }
  }, [load])

  const shown = overview.filter((row) => row.configured)
  // Taken from the full list, so removing one provider does not recolour another.
  const hueOf = (name: string) =>
    hueAt(Math.max(0, overview.map((row) => row.name).indexOf(name)))
  const balances = shown.filter((row) => row.balance !== null && row.basis !== 'usage')
  const total = balances.reduce((sum, row) => sum + (row.balance ?? 0), 0)

  const lastReading = shown
    .map((row) => row.recordedAt)
    .filter((value): value is string => value !== null)
    .sort()
    .at(-1)

  return (
    <div className="flex h-full flex-col bg-surface">
      {/* The popover is frameless, so this strip is both its heading and its title
          bar: drag it to move the popover, and it carries the window controls. The
          dot that used to sit beside the wordmark said what the tray icon already
          says, on the one surface where the icon is inches away. */}
      <div
        data-tauri-drag-region="deep"
        className="flex items-center gap-2 border-b border-line px-3.5 py-2.5"
      >
        <span className="flex flex-1 items-center text-[12px] font-medium">Meterix</span>
        <WindowControls window="popover" />
      </div>

      <div className="flex-1 overflow-y-auto">
        {error ? (
          <div className="num px-3.5 py-3 text-[11px] text-copper">{error}</div>
        ) : shown.length === 0 ? (
          <div className="px-5 py-8 text-center">
            <p className="text-[12px] font-medium">No providers connected</p>
            <p className="num mt-1 text-[11px] text-ink-muted">
              Add a key in the dashboard to start tracking.
            </p>
          </div>
        ) : (
          shown.map((row) => {
            const outcome = outcomes[row.name]
            const failed = outcome !== undefined && !outcome.ok
            const health = healthOf(row, outcome)

            return (
              <div
                key={row.name}
                className="flex items-center gap-2.5 px-3.5 py-2"
                style={{ '--hue': hueOf(row.name) } as CSSProperties}
              >
                {/* Identity, so the row matches the card and the line. This used
                    to be `failed ? copper : teal`, which showed a provider that
                    was merely under its threshold as healthy. */}
                <span className="hue-bar h-[7px] w-[7px] shrink-0 rounded-full" />
                <span className={`num text-[12px] ${failed ? 'text-ink-dim' : ''}`}>
                  {row.displayName}
                </span>
                {health === 'low' ? (
                  <span
                    className={`label-sm rounded-full border px-1.5 py-0.5 ${tones.amber.pill} bg-amber/10`}
                  >
                    Under
                  </span>
                ) : null}
                <span
                  className={`num ml-auto text-[12px] ${failed ? 'text-copper' : 'text-ink-dim'}`}
                >
                  {failed
                    ? 'failed'
                    : row.balance === null
                      ? '—'
                      : usd(row.balance).slice(1)}
                </span>
              </div>
            )
          })
        )}
      </div>

      {/* The total is a summary of the rows above it, so it goes under them rather
          than in the heading. It is also the one figure in here that says what
          currency it is in: the rows are read against it. */}
      <div className="flex shrink-0 items-center gap-2 border-t border-line px-3.5 py-2">
        <span className="label-sm text-ink-dim">Total</span>
        <span className="num ml-auto text-[13px] font-medium">
          {balances.length === 0 ? '—' : usd(total)}
        </span>
      </div>

      <div className="flex items-center gap-2 border-t border-line px-3.5 py-2.5">
        <span className="num text-[10px] text-ink-muted">
          {relativeTime(lastReading ?? null)}
        </span>
        <div className="ml-auto flex gap-1.5">
          <Button
            variant="secondary"
            className="!px-2.5 !py-1 !text-[11px]"
            onClick={() => void invoke('open_dashboard')}
          >
            Dashboard
          </Button>
          <Button
            variant="primary"
            className="!px-2.5 !py-1 !text-[11px]"
            onClick={() => void refresh()}
            disabled={busy}
          >
            {busy ? 'Refreshing' : 'Refresh'}
          </Button>
        </div>
      </div>
    </div>
  )
}
