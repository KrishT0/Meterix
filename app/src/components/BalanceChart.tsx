import type { SnapshotRow } from '../lib/api'
import { parseUtc, usd } from '../lib/format'
import type { Tone } from './ui'

const WIDTH = 1000
const HEIGHT = 170

const strokeOf: Record<Tone, string> = {
  teal: 'var(--color-teal)',
  copper: 'var(--color-copper)',
  amber: 'var(--color-amber)',
  muted: 'var(--color-ink-muted)',
}

const fillOf: Record<Tone, string> = {
  teal: 'var(--color-teal-dim)',
  copper: 'var(--color-copper-dim)',
  amber: 'var(--color-amber-dim)',
  muted: 'var(--color-line-strong)',
}

export interface Series {
  name: string
  tone: Tone
  points: SnapshotRow[]
}

/**
 * Only rows whose `basis` says they are a balance get plotted.
 *
 * A `usage` row holds spend so far, which climbs as money runs out. Drawing it
 * on the same axis as a balance would produce a chart that looks healthy while
 * the account empties, so those rows are excluded and counted instead.
 */
function balancePoints(series: Series[]): { series: Series; points: SnapshotRow[] }[] {
  return series.map((entry) => ({
    series: entry,
    points: entry.points
      .filter((point) => point.basis !== 'usage')
      .slice()
      .sort((a, b) => parseUtc(a.recordedAt).getTime() - parseUtc(b.recordedAt).getTime()),
  }))
}

export function BalanceChart({ series }: { series: Series[] }) {
  const plotted = balancePoints(series)
  const excluded = series.reduce(
    (total, entry) => total + entry.points.filter((point) => point.basis === 'usage').length,
    0,
  )
  const all = plotted.flatMap((entry) => entry.points)

  const legend = (
    <div className="flex flex-wrap items-center gap-4">
      {series.map((entry) => (
        <span key={entry.name} className="flex items-center gap-1.5">
          <span className="h-[6px] w-[6px] rounded-full" style={{ background: strokeOf[entry.tone] }} />
          <span className="label-sm text-ink-dim">{entry.name}</span>
        </span>
      ))}
      {excluded > 0 ? (
        <span className="label-sm text-ink-muted">
          {excluded} usage reading{excluded === 1 ? '' : 's'} excluded
        </span>
      ) : null}
    </div>
  )

  if (all.length < 2) {
    return (
      <div>
        <div className="flex items-center justify-between">
          <span className="label text-ink-muted">Balance over time</span>
          {legend}
        </div>
        <div className="mt-3 rounded-[10px] border border-line bg-inset px-3.5 py-6 text-center">
          <p className="num text-[11px] text-ink-muted">
            {all.length === 0
              ? 'No balance readings stored yet.'
              : 'One reading stored. A trend needs at least two.'}
          </p>
          <p className="num mt-1 text-[10px] text-ink-muted">
            Every refresh adds a point. At a 30 minute poll, a day is about 48 points.
          </p>
        </div>
      </div>
    )
  }

  const times = all.map((point) => parseUtc(point.recordedAt).getTime())
  const tMin = Math.min(...times)
  const tMax = Math.max(...times)
  const vMax = Math.max(1, ...all.map((point) => point.remaining))

  const x = (time: number) => (tMax === tMin ? WIDTH / 2 : ((time - tMin) / (tMax - tMin)) * WIDTH)
  const y = (value: number) => HEIGHT - 14 - (value / vMax) * (HEIGHT - 34)

  return (
    <div>
      <div className="flex items-center justify-between">
        <span className="label text-ink-muted">Balance over time</span>
        {legend}
      </div>

      <svg
        viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
        preserveAspectRatio="none"
        className="mt-3 h-[170px] w-full"
        role="img"
        aria-label="Remaining balance over time"
      >
        <g stroke="var(--color-line)" strokeWidth="1">
          <line x1="0" y1="35" x2={WIDTH} y2="35" />
          <line x1="0" y1="78" x2={WIDTH} y2="78" />
          <line x1="0" y1="121" x2={WIDTH} y2="121" />
        </g>

        {plotted.map((entry) =>
          entry.points.length < 2 ? null : (
            <g key={entry.series.name}>
              <path
                d={`${line(entry.points, x, y)} L ${x(parseUtc(entry.points[entry.points.length - 1]!.recordedAt).getTime())},${HEIGHT} L ${x(parseUtc(entry.points[0]!.recordedAt).getTime())},${HEIGHT} Z`}
                fill={fillOf[entry.series.tone]}
                opacity="0.55"
              />
              <path
                d={line(entry.points, x, y)}
                fill="none"
                stroke={strokeOf[entry.series.tone]}
                strokeWidth="2"
                vectorEffect="non-scaling-stroke"
              />
            </g>
          ),
        )}
      </svg>

      <div className="num mt-2.5 flex items-center justify-between text-[10px] text-ink-muted">
        <span>{new Date(tMin).toLocaleString()}</span>
        <span>peak {usd(vMax)}</span>
        <span>{new Date(tMax).toLocaleString()}</span>
      </div>
    </div>
  )
}

function line(
  points: SnapshotRow[],
  x: (time: number) => number,
  y: (value: number) => number,
): string {
  return points
    .map((point, index) => {
      const px = x(parseUtc(point.recordedAt).getTime())
      const py = y(point.remaining)
      return `${index === 0 ? 'M' : 'L'} ${px.toFixed(1)},${py.toFixed(1)}`
    })
    .join(' ')
}
