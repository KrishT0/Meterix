import type { CSSProperties } from 'react'

import type { ProviderOverview, RefreshOutcome, SnapshotRow } from '../lib/api'
import { basisLabel, errorHeadline, figure, relativeTime, usd } from '../lib/format'
import { CheckIcon, CrossIcon, Label, Pill, WarningIcon, healthOf, tones } from './ui'

const TICKS = 12

/**
 * What is known about the credential, which is a different question from whether
 * the balance is healthy. A working key that is nearly empty still reads
 * "Api key" here, and gets its warning from the status line below.
 */
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
 * One tick per stored reading, oldest first, in the provider's own colour so the
 * card is identifiable at a glance. Grey means the reading held a usage figure
 * rather than a balance, so a run of grey is evidence that no balance is
 * available — which is a fact about the data, not about health, so it stays
 * outside the status colour.
 */
function Ticks({ readings }: { readings: SnapshotRow[] }) {
  const recent = readings.slice(0, TICKS).reverse()
  const padding = Math.max(0, TICKS - recent.length)

  return (
    <div className="flex items-end gap-[2px]" title={`${recent.length} stored reading(s)`}>
      {Array.from({ length: padding }, (_, index) => (
        <span key={`pad-${index}`} className="h-1.5 w-[7px] rounded-[2px] bg-line" />
      ))}
      {recent.map((reading, index) => (
        <span
          key={`${reading.recordedAt}-${index}`}
          className={`h-1.5 w-[7px] rounded-[2px] ${
            reading.basis === 'usage' ? 'bg-ink-muted' : 'hue-bar'
          }`}
        />
      ))}
    </div>
  )
}

/**
 * The status line, and the only place amber or copper appear on a card.
 *
 * Nothing is rendered for a healthy provider. Six cards each announcing "fine"
 * would be six pieces of noise to read past; the absence is the signal, and
 * anything that does appear is worth looking at.
 */
function StatusLabel({
  health,
  threshold,
  outcome,
}: {
  health: ReturnType<typeof healthOf>
  threshold: number
  outcome: RefreshOutcome | undefined
}) {
  if (health === 'error') {
    return (
      <span
        className={`label-sm mt-2 inline-block rounded-full border px-2 py-0.5 ${tones.copper.pill} bg-copper/10`}
      >
        {errorHeadline(outcome?.errorKind)}
      </span>
    )
  }

  if (health === 'low') {
    return (
      <span
        className={`label-sm mt-2 inline-block rounded-full border px-2 py-0.5 ${tones.amber.pill} bg-amber/10`}
      >
        Under your {usd(threshold).slice(1)} threshold
      </span>
    )
  }

  return null
}

export function ProviderCard({
  provider,
  outcome,
  readings,
  hue,
  onRemove,
  busy,
}: {
  provider: ProviderOverview
  outcome: RefreshOutcome | undefined
  readings: SnapshotRow[]
  /** The CSS custom property carrying this provider's identity colour. */
  hue: string
  onRemove: () => void
  busy: boolean
}) {
  const health = healthOf(provider, outcome)
  const pill = pillText(provider, outcome)

  // Shape carries the status, colour carries the provider. A cross and a warning
  // triangle are distinguishable without relying on hue, which is what lets the
  // box stay identity-coloured instead of turning amber or copper.
  const Icon = health === 'ok' ? CheckIcon : health === 'error' ? CrossIcon : WarningIcon

  return (
    <div className="hue-card rounded-[10px] border p-3.5" style={{ '--hue': hue } as CSSProperties}>
      <div className="flex items-center gap-2">
        <span
          className={`flex h-[16px] w-[16px] items-center justify-center rounded-[4px] ${
            health === 'unknown' ? 'border border-line-strong bg-inset' : 'hue-bar'
          }`}
        >
          <Icon className={health === 'unknown' ? 'text-ink-muted' : 'text-[#0F1F1C]'} />
        </span>

        <span className="font-sans text-[13px] font-medium">{provider.displayName}</span>

        <button
          type="button"
          onClick={onRemove}
          disabled={busy || !provider.configured}
          title="Remove this provider's key"
          className="text-ink-muted transition hover:text-ink disabled:cursor-not-allowed disabled:opacity-30"
        >
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
            <path d="M3 6h18M8 6V4h8v2M6 6l1 14h10l1-14" />
          </svg>
        </button>

        <Pill className={`ml-auto ${pill.className}`}>{pill.text}</Pill>
      </div>

      <div className="mt-3 flex items-end justify-between gap-3">
        <div>
          <Label className="text-ink-muted">
            {provider.basis === 'usage'
              ? 'Spend so far'
              : provider.basis === 'quota'
                ? 'Allowance left'
                : 'Remaining'}
          </Label>
          <div className="num mt-0.5 text-[18px] leading-none font-semibold">
            {figure(provider.balance, provider.basis)}
          </div>
        </div>
        <Ticks readings={readings} />
      </div>

      <div className={`label-sm mt-2 ${provider.basis === null ? 'text-ink-muted' : 'hue-ink'}`}>
        {basisLabel(provider.basis)}
      </div>

      <div className="num mt-1 text-[11px] text-ink-dim">
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

      <StatusLabel health={health} threshold={provider.threshold} outcome={outcome} />
    </div>
  )
}
