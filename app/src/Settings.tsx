import { useCallback, useEffect, useState } from 'react'

import { Button, Label, Pill, tones } from './components/ui'
import type { SettingsView } from './lib/api'
import * as api from './lib/api'
import { localTime, shortDate } from './lib/format'

/** The intervals the picker offers, in minutes. */
const INTERVALS = [15, 30, 60, 360]

function intervalLabel(minutes: number): string {
  return minutes >= 60 ? `${minutes / 60}h` : `${minutes}m`
}

/** The one switch shape on this screen, so three rows cannot drift apart. */
function Switch({
  on,
  onToggle,
  label,
}: {
  on: boolean
  onToggle: () => void
  label: string
}) {
  return (
    <button
      type="button"
      onClick={onToggle}
      role="switch"
      aria-checked={on}
      aria-label={label}
      className={`relative mt-0.5 h-[18px] w-[32px] shrink-0 rounded-full transition ${
        on ? 'bg-amber' : 'bg-line-strong'
      }`}
    >
      <span
        className={`absolute top-[2px] h-[14px] w-[14px] rounded-full transition-all ${
          on ? 'right-[2px] bg-[#1A1408]' : 'left-[2px] bg-ink-muted'
        }`}
      />
    </button>
  )
}

/** Everything the form can change. Thresholds stay text so a field can be
 *  mid-edit without a half-typed number being treated as a value. */
type Draft = {
  interval: number
  threshold: string
  notifyLow: boolean
  notifyErrors: boolean
  autostart: boolean
  /** Provider name to its own threshold text. Empty means no override: the
   *  provider's own number where it publishes one, otherwise the app default. */
  providers: Record<string, string>
  /** Provider name to its own interval in minutes. Empty means the app default. */
  intervals: Record<string, string>
}

function draftOf(view: SettingsView): Draft {
  return {
    interval: view.pollIntervalMinutes,
    threshold: view.lowBalanceThreshold.toFixed(2),
    notifyLow: view.notifyLowBalance,
    notifyErrors: view.notifyKeyErrors,
    autostart: view.autostartEnabled,
    providers: Object.fromEntries(
      view.providers.map((provider) => [
        provider.name,
        provider.lowBalanceThreshold?.toFixed(2) ?? '',
      ]),
    ),
    intervals: Object.fromEntries(
      view.providers.map((provider) => [
        provider.name,
        provider.pollIntervalMinutes?.toString() ?? '',
      ]),
    ),
  }
}

/** Compared in cents, because that is all the field shows. Comparing the raw
 *  number would make a stored 5.555 read back as "5.56" and leave the form
 *  open with unsaved changes that the user never made. */
function sameNumber(text: string, value: number): boolean {
  const parsed = Number.parseFloat(text)
  return Number.isFinite(parsed) && Math.round(parsed * 100) === Math.round(value * 100)
}

function differs(view: SettingsView, draft: Draft): boolean {
  if (draft.interval !== view.pollIntervalMinutes) return true
  if (draft.notifyLow !== view.notifyLowBalance) return true
  if (draft.notifyErrors !== view.notifyKeyErrors) return true
  if (draft.autostart !== view.autostartEnabled) return true
  if (!sameNumber(draft.threshold, view.lowBalanceThreshold)) return true

  return view.providers.some((provider) => {
    const text = draft.providers[provider.name] ?? ''
    if (
      provider.lowBalanceThreshold === null
        ? text.trim() !== ''
        : !sameNumber(text, provider.lowBalanceThreshold)
    ) {
      return true
    }

    const minutes = (draft.intervals[provider.name] ?? '').trim()
    const stored = provider.pollIntervalMinutes
    return stored === null ? minutes !== '' : minutes !== String(stored)
  })
}

/**
 * Settings, as a view of the same window rather than a second window.
 *
 * Edits are held in a draft and written by Save Changes, which stays disabled
 * until something actually differs. The per-provider thresholds belong to a
 * separate command, so they are written one at a time once the rest of the form
 * has been accepted, and everything is validated before anything is written.
 */
export function Settings({ onClose }: { onClose: () => void }) {
  const [settings, setSettings] = useState<SettingsView | null>(null)
  const [draft, setDraft] = useState<Draft | null>(null)
  const [problem, setProblem] = useState<string | null>(null)
  const [exported, setExported] = useState<string | null>(null)
  /** Set when the database has been pointed at a new folder this session, which
   *  only takes effect on the next launch. */
  const [moved, setMoved] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  const load = useCallback(async () => {
    try {
      const view = await api.settings()
      setSettings(view)
      setDraft(draftOf(view))
      setProblem(null)
    } catch (error) {
      setProblem(String(error))
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  function edit(patch: Partial<Draft>) {
    setDraft((current) => (current ? { ...current, ...patch } : current))
  }

  function editProvider(name: string, text: string) {
    setDraft((current) =>
      current ? { ...current, providers: { ...current.providers, [name]: text } } : current,
    )
  }

  function editInterval(name: string, text: string) {
    setDraft((current) =>
      current ? { ...current, intervals: { ...current.intervals, [name]: text } } : current,
    )
  }

  async function exportCsv() {
    try {
      setExported(await api.exportHistory())
      setProblem(null)
    } catch (error) {
      setProblem(String(error))
    }
  }

  /** Both pickers write a real path on disk, so a refusal has to be shown rather
   *  than swallowed — the folder picker is the only way to change either. */
  async function chooseFolder(kind: 'data' | 'export') {
    try {
      const chosen =
        kind === 'data' ? await api.setDataDirectory() : await api.setExportDirectory()
      if (chosen === null) return

      setProblem(null)
      if (kind === 'data') setMoved(chosen)
      else setExported(null)
      await load()
    } catch (error) {
      setProblem(String(error))
    }
  }

  async function save() {
    if (!settings || !draft) return

    const threshold = Number.parseFloat(draft.threshold)
    if (!Number.isFinite(threshold) || threshold < 0) {
      setProblem('the default threshold must be a number of dollars, zero or more')
      return
    }

    // Collected and checked first, so one bad row cannot leave the form half
    // written.
    const overrides: { name: string; value: number | null }[] = []
    const intervalOverrides: { name: string; minutes: number | null }[] = []

    for (const provider of settings.providers) {
      const text = (draft.providers[provider.name] ?? '').trim()
      const stored = provider.lowBalanceThreshold

      if (text === '') {
        // Blank clears the override, which is not a threshold of zero.
        if (stored !== null) overrides.push({ name: provider.name, value: null })
      } else {
        const parsed = Number.parseFloat(text)
        if (!Number.isFinite(parsed) || parsed < 0) {
          setProblem(`${provider.displayName}: enter a number of dollars, zero or more`)
          return
        }

        if (parsed !== stored) overrides.push({ name: provider.name, value: parsed })
      }

      const minutesText = (draft.intervals[provider.name] ?? '').trim()
      const storedMinutes = provider.pollIntervalMinutes

      if (minutesText === '') {
        if (storedMinutes !== null) {
          intervalOverrides.push({ name: provider.name, minutes: null })
        }
        continue
      }

      const minutes = Number(minutesText)
      // Whole minutes and at least one: the poller checks on a beat, and a zero
      // here would mean a fetch every beat against a paid endpoint.
      if (!Number.isInteger(minutes) || minutes < 1) {
        setProblem(`${provider.displayName}: enter whole minutes, one or more`)
        return
      }

      if (minutes !== storedMinutes) {
        intervalOverrides.push({ name: provider.name, minutes })
      }
    }

    setSaving(true)
    try {
      await api.saveSettings(
        {
          pollIntervalMinutes: draft.interval,
          lowBalanceThreshold: threshold,
          notifyLowBalance: draft.notifyLow,
          notifyKeyErrors: draft.notifyErrors,
        },
        draft.autostart,
      )

      for (const override of overrides) {
        await api.setProviderThreshold(override.name, override.value)
      }

      for (const override of intervalOverrides) {
        await api.setProviderInterval(override.name, override.minutes)
      }

      await load()
    } catch (error) {
      setProblem(String(error))
    } finally {
      setSaving(false)
    }
  }

  if (!settings || !draft) {
    return (
      <div className="flex-1 p-4">
        <Label className="text-ink-muted">{problem ? 'Could not read settings' : 'Loading'}</Label>
        {problem ? <p className="num mt-2 text-[11px] text-copper">{problem}</p> : null}
      </div>
    )
  }

  const dirty = differs(settings, draft)

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
                Applied to every provider without an interval of its own. Shorter intervals mean
                more requests to each one.
              </div>
            </div>
            <div className="flex shrink-0 rounded-lg border border-line-strong bg-inset p-0.5">
              {INTERVALS.map((minutes) => (
                <button
                  key={minutes}
                  type="button"
                  onClick={() => edit({ interval: minutes })}
                  aria-pressed={draft.interval === minutes}
                  className={`label-sm rounded-md px-2.5 py-1 transition ${
                    draft.interval === minutes
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
                Used for a provider that has no threshold of its own and publishes none. Turns
                the tray amber.
              </div>
            </div>
            <div className="flex shrink-0 items-center gap-2 rounded-lg border border-line bg-inset px-3 py-2">
              <span className="label text-ink-muted">$</span>
              <input
                value={draft.threshold}
                onChange={(event) => edit({ threshold: event.target.value })}
                onKeyDown={(event) => {
                  if (event.key === 'Enter') void save()
                }}
                className="num w-[54px] bg-transparent text-right text-[12px] text-ink outline-none"
              />
            </div>
          </div>

          <div className="flex items-start justify-between gap-8 py-3">
            <div>
              <div className="text-[12px]">Notify when a balance drops below its threshold</div>
              <div className="num mt-1 text-[11px] text-ink-muted">
                Once, at the moment it crosses. The tray stays amber until you top up.
              </div>
            </div>
            <Switch
              on={draft.notifyLow}
              onToggle={() => edit({ notifyLow: !draft.notifyLow })}
              label="Notify when a balance drops below its threshold"
            />
          </div>

          <div className="flex items-start justify-between gap-8 py-3">
            <div>
              <div className="text-[12px]">Notify when a key stops working</div>
              <div className="num mt-1 text-[11px] text-ink-muted">
                A rejected key means this provider has quietly stopped being tracked.
              </div>
            </div>
            <Switch
              on={draft.notifyErrors}
              onToggle={() => edit({ notifyErrors: !draft.notifyErrors })}
              label="Notify when a key stops working"
            />
          </div>
        </div>
      </div>

      {/* zone: providers */}
      <div className="mt-10">
        <Label className="text-ink-muted">Providers</Label>

        {settings.providers.length === 0 ? (
          <div className="mt-3 rounded-[10px] border border-line bg-inset px-3.5 py-2.5">
            <span className="num text-[11px] text-ink-muted">
              No providers connected. Save a key on the dashboard and it appears here.
            </span>
          </div>
        ) : (
          <div className="mt-3 border-y border-line">
            <div className="grid grid-cols-[1.2fr_1fr_0.6fr_0.7fr_0.8fr] gap-3 border-b border-line pb-2 pt-2.5">
              <span className="label-sm text-ink-muted">Provider</span>
              <span className="label-sm text-ink-muted">Key</span>
              <span className="label-sm text-right text-ink-muted">Since</span>
              <span className="label-sm text-right text-ink-muted">Every</span>
              <span className="label-sm text-right text-ink-muted">Low at</span>
            </div>

            {settings.providers.map((provider) => (
              <div
                key={provider.name}
                className="grid grid-cols-[1.2fr_1fr_0.6fr_0.7fr_0.8fr] items-center gap-3 border-b border-line/60 py-2.5 last:border-b-0"
              >
                <span className="num text-[12px]">{provider.displayName}</span>
                <span className="num truncate text-[11px] text-ink-dim">{provider.keyHint}</span>
                {/* The earliest reading, not when the key was saved: it is the
                    date the database can actually back up. */}
                <span
                  className="num text-right text-[11px] text-ink-dim"
                  title={
                    provider.firstReadingAt === null
                      ? 'No readings stored yet'
                      : `Tracking since ${localTime(provider.firstReadingAt)}`
                  }
                >
                  {shortDate(provider.firstReadingAt)}
                </span>
                {/* Minutes, because the interval is what a rate-limited
                    provider needs raised without slowing the others down. */}
                <div className="flex items-center justify-end gap-1">
                  <input
                    value={draft.intervals[provider.name] ?? ''}
                    placeholder={String(draft.interval)}
                    onChange={(event) => editInterval(provider.name, event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === 'Enter') void save()
                    }}
                    className="num w-[44px] rounded-md border border-line bg-inset px-1.5 py-1 text-right text-[12px] text-ink outline-none placeholder:text-ink-muted"
                  />
                  <span className="label text-ink-muted">m</span>
                </div>
                <div className="flex items-center justify-end gap-1.5">
                  <span className="label text-ink-muted">$</span>
                  <input
                    value={draft.providers[provider.name] ?? ''}
                    placeholder={provider.fallbackThreshold.toFixed(2)}
                    onChange={(event) => editProvider(provider.name, event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === 'Enter') void save()
                    }}
                    className="num w-[54px] rounded-md border border-line bg-inset px-1.5 py-1 text-right text-[12px] text-ink outline-none placeholder:text-ink-muted"
                  />
                </div>
              </div>
            ))}

            <div className="border-t border-line py-2">
              <span className="num text-[11px] text-ink-muted">
                Leave a provider blank to use its own default. Each placeholder shows what
                that is: the balance the provider itself calls low, or the default above for a
                provider that publishes none. An empty interval follows the polling default
                above, and the poller wakes every 30 seconds to check what is due.
              </span>
            </div>
          </div>
        )}
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
            <Switch
              on={draft.autostart}
              onToggle={() => edit({ autostart: !draft.autostart })}
              label="Launch at login"
            />
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
                {moved ?? settings.databasePath}
              </div>
              {/* Said plainly, because a folder that does not appear to do anything
                  until the next launch reads as a button that failed. */}
              {moved ? (
                <div className="num mt-1 text-[11px] text-amber-dim">
                  Readings are being copied there. Restart Meterix to use it.
                </div>
              ) : null}
            </div>
            <Button
              variant="secondary"
              className="shrink-0"
              onClick={() => void chooseFolder('data')}
            >
              Change folder
            </Button>
          </div>

          <div className="flex items-start justify-between gap-8 py-3">
            <div className="min-w-0">
              <div className="text-[12px]">Reading history</div>
              {/* The path replaces the explanation once there is one, so the row
                  says where the file went rather than what the button does. */}
              <div className="num mt-1 truncate text-[11px] text-ink-muted">
                {exported ?? 'Every stored reading as CSV, written to the folder below.'}
              </div>
            </div>
            <div className="flex shrink-0 items-center gap-2">
              <Button variant="secondary" onClick={() => void chooseFolder('export')}>
                Change folder
              </Button>
              <Button variant="secondary" onClick={() => void exportCsv()}>
                Export CSV
              </Button>
            </div>
          </div>

          <div className="flex items-start justify-between gap-8 py-3">
            <div className="min-w-0">
              <div className="text-[12px]">Exports go to</div>
              <div className="num mt-1 truncate text-[11px] text-ink-muted">
                {settings.exportDirectory}
              </div>
            </div>
          </div>
        </div>
      </div>

      {/* zone: footer.
          The rule runs edge to edge like the window's own header rule instead of
          stopping at this view's padding: `-mx-4` cancels the `p-4` above and
          `px-4` puts the content back where it was. */}
      <div className="mt-10 -mx-4 flex items-center gap-3 border-t border-line px-4 pt-4">
        {dirty ? (
          <Pill className={tones.amber.pill}>unsaved changes</Pill>
        ) : (
          <span className="num text-[11px] text-ink-muted">all changes saved</span>
        )}
        <div className="ml-auto flex items-center gap-2">
          <Button variant="secondary" onClick={onClose}>
            Back to dashboard
          </Button>
          <Button variant="primary" disabled={!dirty || saving} onClick={() => void save()}>
            {saving ? 'Saving…' : 'Save Changes'}
          </Button>
        </div>
      </div>
    </div>
  )
}
