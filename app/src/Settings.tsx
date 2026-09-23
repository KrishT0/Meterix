import { useCallback, useEffect, useState } from 'react'

import { Button, Label, Pill, tones } from './components/ui'
import type { SettingsView } from './lib/api'
import * as api from './lib/api'

/** The intervals the picker offers, in minutes. */
const INTERVALS = [15, 30, 60, 360]

function intervalLabel(minutes: number): string {
  return minutes >= 60 ? `${minutes / 60}h` : `${minutes}m`
}

/**
 * Settings, as a view of the same window rather than a second window.
 *
 * Everything here is saved on change: there is no Save button, because there is
 * nothing to batch and a form that saves itself is one fewer thing to get wrong.
 */
export function Settings({ onClose }: { onClose: () => void }) {
  const [settings, setSettings] = useState<SettingsView | null>(null)
  const [draftInterval, setDraftInterval] = useState<number | null>(null)
  const [draftThreshold, setDraftThreshold] = useState('')
  const [autostart, setAutostart] = useState(false)
  const [problem, setProblem] = useState<string | null>(null)
  const [note, setNote] = useState<string | null>(null)

  const load = useCallback(async () => {
    try {
      const view = await api.settings()
      setSettings(view)
      setDraftInterval(view.pollIntervalMinutes)
      setDraftThreshold(view.lowBalanceThreshold.toFixed(2))
      setAutostart(view.autostartEnabled)
      setProblem(null)
    } catch (error) {
      setProblem(String(error))
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  /** Writes the whole form, because the command saves it as one. */
  const persist = useCallback(
    async (next: { interval: number; threshold: number; autostart: boolean }) => {
      try {
        await api.saveSettings(
          {
            pollIntervalMinutes: next.interval,
            lowBalanceThreshold: next.threshold,
          },
          next.autostart,
        )
        setNote('saved')
        await load()
      } catch (error) {
        setProblem(String(error))
        setNote(null)
      }
    },
    [load],
  )

  function chooseInterval(minutes: number) {
    setDraftInterval(minutes)
    if (settings) {
      void persist({
        interval: minutes,
        threshold: settings.lowBalanceThreshold,
        autostart,
      })
    }
  }

  function commitThreshold() {
    if (!settings) return

    const parsed = Number.parseFloat(draftThreshold)
    if (!Number.isFinite(parsed) || parsed < 0) {
      setProblem('threshold must be a number of dollars, zero or more')
      setDraftThreshold(settings.lowBalanceThreshold.toFixed(2))
      return
    }

    void persist({
      interval: settings.pollIntervalMinutes,
      threshold: parsed,
      autostart,
    })
  }

  function toggleAutostart() {
    if (!settings) return

    const next = !autostart
    setAutostart(next)
    void persist({
      interval: settings.pollIntervalMinutes,
      threshold: settings.lowBalanceThreshold,
      autostart: next,
    })
  }

  /** Thresholds are written per row, so they get their own call. */
  async function setProviderThreshold(provider: string, raw: string) {
    const trimmed = raw.trim()

    try {
      if (trimmed === '') {
        // Empty means "use the app default", which is not a threshold of zero.
        await api.setProviderThreshold(provider, null)
      } else {
        const parsed = Number.parseFloat(trimmed)
        if (!Number.isFinite(parsed) || parsed < 0) {
          setProblem('threshold must be a number of dollars, zero or more')
          return
        }
        await api.setProviderThreshold(provider, parsed)
      }

      setNote('saved')
      await load()
    } catch (error) {
      setProblem(String(error))
    }
  }

  if (!settings) {
    return (
      <div className="flex-1 p-4">
        <Label className="text-ink-muted">{problem ? 'Could not read settings' : 'Loading'}</Label>
        {problem ? <p className="num mt-2 text-[11px] text-copper">{problem}</p> : null}
      </div>
    )
  }

  return (
    <div className="flex-1 p-4">
      {problem ? (
        <div className="mb-4 rounded-lg border border-copper-dim/40 bg-copper-tint px-3.5 py-2.5">
          <span className="num text-[11px] text-copper">{problem}</span>
        </div>
      ) : null}

      {/* zone: polling */}
      <div>
        <Label className="text-ink-muted">Polling</Label>

        <div className="mt-3 divide-y divide-line/60 border-y border-line">
          <div className="flex items-start justify-between gap-8 py-3">
            <div>
              <div className="text-[12px]">Check balances every</div>
              <div className="num mt-1 text-[11px] text-ink-muted">
                Applied to every provider. Shorter intervals mean more requests to each one.
              </div>
            </div>
            <div className="flex shrink-0 rounded-lg border border-line-strong bg-inset p-0.5">
              {INTERVALS.map((minutes) => (
                <button
                  key={minutes}
                  type="button"
                  onClick={() => chooseInterval(minutes)}
                  aria-pressed={draftInterval === minutes}
                  className={`label-sm rounded-md px-2.5 py-1 transition ${
                    draftInterval === minutes
                      ? 'bg-amber text-[#1A1408]'
                      : 'text-ink-dim hover:text-ink'
                  }`}
                >
                  {intervalLabel(minutes)}
                </button>
              ))}
            </div>
          </div>

          <div className="flex items-start justify-between gap-8 py-3">
            <div>
              <div className="text-[12px]">Keep polling when the dashboard is closed</div>
              <div className="num mt-1 text-[11px] text-ink-muted">
                Always on. Closing the window hides it; the tray keeps running.
              </div>
            </div>
            <span className="label-sm mt-1 shrink-0 text-ink-muted">always on</span>
          </div>
        </div>
      </div>

      {/* zone: alerts */}
      <div className="mt-10">
        <Label className="text-ink-muted">Alerts</Label>

        <div className="mt-3 divide-y divide-line/60 border-y border-line">
          <div className="flex items-start justify-between gap-8 py-3">
            <div>
              <div className="text-[12px]">Default low balance</div>
              <div className="num mt-1 text-[11px] text-ink-muted">
                Used for any provider without its own. Turns the tray amber.
              </div>
            </div>
            <div className="flex shrink-0 items-center gap-2 rounded-lg border border-line bg-inset px-3 py-2">
              <span className="label text-ink-muted">$</span>
              <input
                value={draftThreshold}
                onChange={(event) => setDraftThreshold(event.target.value)}
                onBlur={commitThreshold}
                onKeyDown={(event) => {
                  if (event.key === 'Enter') commitThreshold()
                }}
                className="num w-[54px] bg-transparent text-right text-[12px] text-ink outline-none"
              />
            </div>
          </div>
        </div>
      </div>

      {/* zone: providers */}
      <div className="mt-10">
        <Label className="text-ink-muted">Providers</Label>

        <div className="mt-3 border-y border-line">
          <div className="grid grid-cols-[1.4fr_1.1fr_0.9fr] gap-3 border-b border-line pb-2 pt-2.5">
            <span className="label-sm text-ink-muted">Provider</span>
            <span className="label-sm text-ink-muted">Key</span>
            <span className="label-sm text-right text-ink-muted">Low at</span>
          </div>

          {settings.providers.map((provider) => (
            <div
              key={provider.name}
              className="grid grid-cols-[1.4fr_1.1fr_0.9fr] items-center gap-3 border-b border-line/60 py-2.5 last:border-b-0"
            >
              <span className="num text-[12px]">{provider.displayName}</span>
              <span className="num truncate text-[11px] text-ink-dim">
                {provider.keyHint ?? <span className="text-ink-muted">no key</span>}
              </span>
              <div className="flex items-center justify-end gap-1.5">
                <span className="label text-ink-muted">$</span>
                <input
                  defaultValue={provider.lowBalanceThreshold?.toFixed(2) ?? ''}
                  placeholder={settings.lowBalanceThreshold.toFixed(2)}
                  onBlur={(event) => void setProviderThreshold(provider.name, event.target.value)}
                  onKeyDown={(event) => {
                    if (event.key === 'Enter') event.currentTarget.blur()
                  }}
                  className="num w-[54px] rounded-md border border-line bg-inset px-1.5 py-1 text-right text-[12px] text-ink outline-none placeholder:text-ink-muted"
                />
              </div>
            </div>
          ))}

          <div className="border-t border-line py-2">
            <span className="num text-[11px] text-ink-muted">
              Leave a provider blank to use the default. Its placeholder shows what that is.
            </span>
          </div>
        </div>
      </div>

      {/* zone: startup */}
      <div className="mt-10">
        <Label className="text-ink-muted">Startup</Label>

        <div className="mt-3 divide-y divide-line/60 border-y border-line">
          <div className="flex items-start justify-between gap-8 py-3">
            <div>
              <div className="text-[12px]">Launch at login</div>
              <div className="num mt-1 text-[11px] text-ink-muted">
                Starts in the tray, without opening the dashboard.
              </div>
            </div>
            <button
              type="button"
              onClick={toggleAutostart}
              role="switch"
              aria-checked={autostart}
              aria-label="Launch at login"
              className={`relative mt-0.5 h-[18px] w-[32px] shrink-0 rounded-full transition ${
                autostart ? 'bg-amber' : 'bg-line-strong'
              }`}
            >
              <span
                className={`absolute top-[2px] h-[14px] w-[14px] rounded-full transition-all ${
                  autostart ? 'right-[2px] bg-[#1A1408]' : 'left-[2px] bg-ink-muted'
                }`}
              />
            </button>
          </div>
        </div>
      </div>

      {/* zone: data */}
      <div className="mt-10">
        <Label className="text-ink-muted">Data</Label>

        <div className="mt-3 divide-y divide-line/60 border-y border-line">
          <div className="flex items-start justify-between gap-8 py-3">
            <div className="min-w-0">
              <div className="text-[12px]">Database</div>
              <div className="num mt-1 truncate text-[11px] text-ink-muted">
                {settings.databasePath}
              </div>
            </div>
            <Button
              variant="secondary"
              className="shrink-0"
              onClick={() => void navigator.clipboard.writeText(settings.databasePath)}
            >
              Copy path
            </Button>
          </div>
        </div>
      </div>

      {/* zone: footer */}
      <div className="mt-10 flex items-center gap-3">
        {note ? (
          <Pill className={tones.teal.pill}>{note}</Pill>
        ) : (
          <span className="num text-[11px] text-ink-muted">changes are saved as you make them</span>
        )}
        <div className="ml-auto">
          <Button variant="secondary" onClick={onClose}>
            Back to dashboard
          </Button>
        </div>
      </div>
    </div>
  )
}
