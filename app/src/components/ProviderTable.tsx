import type { CSSProperties } from 'react'

import type { ProviderOverview, RefreshOutcome } from '../lib/api'
import { basisLabel, errorHeadline, figure, relativeTime, usd } from '../lib/format'
import { healthOf, tones } from './ui'

const COLUMNS = 'grid-cols-[1.5fr_1.2fr_0.9fr_0.9fr_0.9fr]'

/**
 * Status, in the one place on a row where amber and copper are allowed.
 *
 * It sits beside the name rather than replacing the timestamp, so a low provider
 * still shows when it was last read — the chip answers "is this fine" and the
 * time answers "how current is that".
 */
function StatusChip({
  health,
  outcome,
}: {
  health: ReturnType<typeof healthOf>
  outcome: RefreshOutcome | undefined
}) {
  if (health === 'error') {
    return (
      <span className={`label-sm rounded-full border px-1.5 py-0.5 ${tones.copper.pill} bg-copper/10`}>
        {errorHeadline(outcome?.errorKind)}
      </span>
    )
  }
  if (health === 'low') {
    return (
      <span className={`label-sm rounded-full border px-1.5 py-0.5 ${tones.amber.pill} bg-amber/10`}>
        Under
      </span>
    )
  }
  return null
}

export function ProviderTable({
  providers,
  outcomes,
  hueOf,
}: {
  providers: ProviderOverview[]
  outcomes: Record<string, RefreshOutcome>
  /** The CSS custom property carrying each provider's identity colour. */
  hueOf: (name: string) => string
}) {
  if (providers.length === 0) return null

  return (
    <div>
      <div className={`grid ${COLUMNS} gap-3 border-b border-line pb-2`}>
        <span className="label text-ink-muted">Provider</span>
        <span className="label text-ink-muted">Basis</span>
        <span className="label text-right text-ink-muted">Balance</span>
        <span className="label text-right text-ink-muted">Spend</span>
        <span className="label text-right text-ink-muted">Checked</span>
      </div>

      {providers.map((provider) => {
        const outcome = outcomes[provider.name]
        const health = healthOf(provider, outcome)
        const failed = outcome !== undefined && !outcome.ok

        return (
          <div
            key={provider.name}
            className={`grid ${COLUMNS} items-center gap-3 border-b border-line/60 py-2 last:border-b-0`}
            style={{ '--hue': hueOf(provider.name) } as CSSProperties}
          >
            <span className="flex items-center gap-2">
              {/* Identity, so a row matches its card and its line on the chart. */}
              <span className="hue-bar h-[7px] w-[7px] shrink-0 rounded-full" />
              <span className="num text-[12px]">{provider.displayName}</span>
              <StatusChip health={health} outcome={outcome} />
            </span>

            <span
              className={`label-sm ${provider.basis === null ? 'text-ink-muted' : provider.basis === 'usage' ? 'text-ink-dim' : 'text-ink-dim'}`}
            >
              {failed ? '—' : (provider.basis ?? '—')}
            </span>

            <span className="num text-right text-[12px]">
              {figure(provider.balance, provider.basis)}
            </span>

            <span className="num text-right text-[12px] text-ink-dim">
              {provider.usage === null ? (
                '—'
              ) : (
                <>
                  {usd(provider.usage).slice(1)}
                  {provider.spendWindowDays === null ? null : (
                    <span className="text-ink-muted">/{provider.spendWindowDays}d</span>
                  )}
                </>
              )}
            </span>

            <span
              className={`num text-right text-[12px] text-ink-muted`}
              title={failed ? (outcome.errorMessage ?? undefined) : basisLabel(provider.basis)}
            >
              {relativeTime(provider.recordedAt)}
            </span>
          </div>
        )
      })}
    </div>
  )
}
