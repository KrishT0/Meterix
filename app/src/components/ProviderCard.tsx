import type { ProviderOverview, RefreshOutcome, SnapshotRow } from '../lib/api'
import { basisLabel, errorHeadline, relativeTime, usd } from '../lib/format'
import {
  CheckIcon,
  CrossIcon,
  Label,
  Pill,
  StatusDot,
  WarningIcon,
  healthOf,
  toneOf,
  tones,
} from './ui'

const TICKS = 12

function pillText(
  provider: ProviderOverview,
  outcome: RefreshOutcome | undefined,
): { text: string; className: string } {
  if (outcome && !outcome.ok) {
    return { text: errorHeadline(outcome.errorKind), className: tones.copper.pill }
  }
  if (!provider.configured) {
    return { text: 'No key', className: tones.muted.pill }
  }
  if (provider.basis === 'usage') {
    return { text: 'Usage only', className: tones.muted.pill }
  }
  if (provider.balance === null) {
    return { text: 'No reading', className: tones.muted.pill }
  }
  return { text: 'Api key', className: tones.muted.pill }
}

/**
 * One tick per stored reading, oldest first. A tick is teal when the reading
 * was a real balance and grey when it was only a usage figure, so a run of
 * grey is visible evidence that no balance is available for this provider.
 */
function Ticks({ readings, tone }: { readings: SnapshotRow[]; tone: string }) {
  const recent = readings.slice(0, TICKS).reverse()
  const padding = Math.max(0, TICKS - recent.length)

  return (
    <div className="flex items-end gap-[3px]" title={`${recent.length} stored reading(s)`}>
      {Array.from({ length: padding }, (_, index) => (
        <span key={`pad-${index}`} className="h-2 w-[9px] rounded-[3px] bg-line" />
      ))}
      {recent.map((reading, index) => (
        <span
          key={`${reading.recordedAt}-${index}`}
          className={`h-2 w-[9px] rounded-[3px] ${reading.basis === 'usage' ? 'bg-ink-muted' : tone}`}
        />
      ))}
    </div>
  )
}

export function ProviderCard({
  provider,
  outcome,
  readings,
  onRemove,
  busy,
}: {
  provider: ProviderOverview
  outcome: RefreshOutcome | undefined
  readings: SnapshotRow[]
  onRemove: () => void
  busy: boolean
}) {
  const health = healthOf(provider, outcome)
  const tone = toneOf(health)
  const style = tones[tone]
  const pill = pillText(provider, outcome)

  const Icon = health === 'ok' ? CheckIcon : health === 'error' ? CrossIcon : WarningIcon

  return (
    <div className={`rounded-[12px] border p-4 ${style.card}`}>
      <div className="flex items-center gap-2.5">
        <span
          className={`flex h-[18px] w-[18px] items-center justify-center rounded-[5px] ${
            health === 'unknown' ? 'border border-line-strong bg-inset' : style.iconBox
          }`}
        >
          <Icon className={health === 'unknown' ? 'text-ink-muted' : 'text-[#0F1F1C]'} />
        </span>

        <span className="text-[15px] font-medium capitalize">{provider.name}</span>

        <button
          type="button"
          onClick={onRemove}
          disabled={busy || !provider.configured}
          title="Remove this provider's key"
          className="text-ink-muted transition hover:text-ink disabled:cursor-not-allowed disabled:opacity-30"
        >
          <svg
            width="13"
            height="13"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.9"
            strokeLinecap="round"
            strokeLinejoin="round"
            aria-hidden="true"
          >
            <path d="M3 6h18M8 6V4h8v2M6 6l1 14h10l1-14" />
          </svg>
        </button>

        <Pill className={`ml-auto ${pill.className}`}>{pill.text}</Pill>
      </div>

      <div className="mt-4 flex items-end justify-between gap-4">
        <div>
          <Label className="text-ink-muted">
            {provider.basis === 'usage' ? 'Spend so far' : 'Remaining'}
          </Label>
          <div
            className={`num mt-1 text-[22px] leading-none font-semibold ${
              tone === 'muted' ? 'text-ink-muted' : style.text
            }`}
          >
            {provider.balance === null ? '——' : usd(provider.balance).slice(1)}
          </div>
        </div>
        <Ticks readings={readings} tone={style.dot} />
      </div>

      <div className={`label-sm mt-3 ${tone === 'muted' ? 'text-ink-muted' : style.text}`}>
        {basisLabel(provider.basis)}
      </div>

      <div className="num mt-1.5 text-[12px] text-ink-dim">
        {outcome && !outcome.ok ? (
          <span title={outcome.errorMessage ?? undefined}>{outcome.errorMessage}</span>
        ) : (
          <>
            {provider.usage === null ? null : (
              <>
                usage {usd(provider.usage).slice(1)}
                {provider.spendWindowDays === null ? '' : `/${provider.spendWindowDays}d`} ·{' '}
              </>
            )}
            checked {relativeTime(provider.recordedAt)}
          </>
        )}
      </div>

      {outcome && !outcome.ok ? null : (
        <div className="mt-2 flex items-center gap-1.5">
          <StatusDot tone={tone} />
          <span className="label-sm text-ink-muted">
            {provider.basis === 'account_credits' && provider.accountCredits !== null
              ? `credits ${usd(provider.accountCredits).slice(1)}`
              : 'no account balance reported'}
          </span>
        </div>
      )}
    </div>
  )
}
