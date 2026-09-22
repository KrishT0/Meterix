import type { ProviderOverview, RefreshOutcome } from '../lib/api'
import { basisLabel, errorHeadline, relativeTime, usd } from '../lib/format'
import { StatusDot, healthOf, toneOf, tones } from './ui'

const COLUMNS = 'grid-cols-[1.5fr_1.2fr_0.9fr_0.9fr_0.9fr]'

export function ProviderTable({
  providers,
  outcomes,
}: {
  providers: ProviderOverview[]
  outcomes: Record<string, RefreshOutcome>
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
        const tone = toneOf(health)
        const failed = outcome !== undefined && !outcome.ok

        return (
          <div
            key={provider.name}
            className={`grid ${COLUMNS} items-center gap-3 border-b border-line/60 py-2 last:border-b-0`}
          >
            <span className="flex items-center gap-2">
              <StatusDot tone={tone} />
              <span className="num text-[12px]">{provider.displayName}</span>
            </span>

            <span
              className={`label-sm ${provider.basis === null ? 'text-ink-muted' : provider.basis === 'usage' ? 'text-ink-dim' : 'text-teal-dim'}`}
            >
              {failed ? '—' : (provider.basis ?? '—')}
            </span>

            <span className={`num text-right text-[12px] ${tone === 'muted' ? 'text-ink-muted' : tones[tone].text}`}>
              {provider.balance === null ? '—' : usd(provider.balance).slice(1)}
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
              className={`num text-right text-[12px] ${failed ? tones.copper.text : 'text-ink-muted'}`}
              title={failed ? (outcome.errorMessage ?? undefined) : basisLabel(provider.basis)}
            >
              {failed ? errorHeadline(outcome.errorKind) : relativeTime(provider.recordedAt)}
            </span>
          </div>
        )
      })}
    </div>
  )
}
