import { useState } from 'react'

import type { SnapshotRow } from '../lib/api'
import { parseUtc, usd } from '../lib/format'

const WIDTH = 1000
/**
 * Tall enough for the hover panel to sit inside it. The panel carries a date,
 * one row per provider and a combined total, which is around 105px, so a 150px
 * plot had nowhere to put it without clipping.
 */
const HEIGHT = 180
/** Room for the y-axis on the left, and for the end-of-line values on the right. */
const AXIS_WIDTH = 46
const END_WIDTH = 54

export interface Series {
  name: string
  /** How the provider is written for a person. The id is not a label. */
  displayName: string
  /**
   * The CSS custom property carrying this provider's identity colour, set by the
   * caller so the line matches that provider's card and its row in the table.
   *
   * Health is deliberately not encoded here. It used to be, with the first
   * series teal and every other one copper, which meant five providers drew four
   * identical copper lines — and copper already means "a key was rejected".
   */
  hue: string
  points: SnapshotRow[]
  /**
   * The credential currently configured for this provider, so readings from a
   * different account are not drawn as one continuous line.
   */
  keyFingerprint: string | null
}

const TICK_TARGET = 4
/** The window background, so labels sitting over a line stay readable. */
const HALO = '0 0 5px var(--color-surface), 0 0 5px var(--color-surface)'

/**
 * A point as an SVG coordinate pair.
 *
 * Two decimals, which is already finer than anything a 1000-unit-wide plot can
 * show, and it keeps the curve's control points close enough to exact that a
 * reading is not nudged by the writing of it.
 */
function placed(point: [number, number]): string {
  return `${point[0].toFixed(2)},${point[1].toFixed(2)}`
}

/**
 * A number from a list the loops above have already filled.
 *
 * The fallback only satisfies the compiler's check on indexing, which is on in
 * this project. Every call is in range, and a missing slope reading as flat is the
 * honest answer for a gap that is not there.
 */
function numberAt(list: number[], index: number): number {
  return list[index] ?? 0
}

/**
 * A smooth line through every reading, which never invents a reading.
 *
 * Straight segments between points looked like a machine measuring at intervals,
 * so the line is curved with monotone cubic interpolation (Fritsch–Carlson): the
 * tangent at each point starts as the average of the two slopes meeting there,
 * then gets pulled back wherever it would carry the curve past the readings either
 * side of it. A plain spline through the same points is smoother still, but it
 * overshoots, and a balance that fell from 40 to 12 would dip below 12 on the way.
 * On a chart of measured money that reads as a reading that never happened.
 *
 * Points arrive as [x, y] in plot units, already placed.
 */
function smoothPath(points: [number, number][]): string {
  const first = points[0]
  const second = points[1]

  if (!first) return ''
  // With two readings there is nothing between them to bend, and any curve would
  // be invented detail.
  if (!second) return `M ${placed(first)}`
  if (points.length === 2) return `M ${placed(first)} L ${placed(second)}`

  const count = points.length
  /** Horizontal gap between readings, and the slope across it. */
  const gap: number[] = []
  const slope: number[] = []
  let previous: [number, number] | undefined

  for (const point of points) {
    if (previous) {
      const width = point[0] - previous[0]
      gap.push(width)
      slope.push(width === 0 ? 0 : (point[1] - previous[1]) / width)
    }

    previous = point
  }

  const tangent: number[] = new Array(count).fill(0)
  tangent[0] = numberAt(slope, 0)
  tangent[count - 1] = numberAt(slope, count - 2)

  for (let i = 1; i < count - 1; i += 1) {
    const before = numberAt(slope, i - 1)
    const after = numberAt(slope, i)

    // A reading that turns around, rising before it and falling after, gets a flat
    // tangent. Averaging there would point the curve whichever way the larger slope
    // went, and it would leave the reading in the wrong direction before turning
    // back, drawing a peak or a dip that was never measured. This rule is what
    // actually bounds the curve; the averaging below it does not, on its own. It
    // was missing at first and the check caught it: overshoot came out proportional
    // to the size of the neighbouring jump.
    tangent[i] = before * after > 0 ? (before + after) / 2 : 0
  }

  for (let i = 0; i < count - 1; i += 1) {
    const steep = numberAt(slope, i)

    if (steep === 0) {
      // Two equal readings stay equal between them. Anything else bulges off a
      // flat stretch, which is where a straight line looked most obviously wrong.
      tangent[i] = 0
      tangent[i + 1] = 0
      continue
    }

    // Scaling both tangents of a segment by the same factor keeps the curve inside
    // the readings it joins. Past nine the segment would double back on itself,
    // which is the whole reason the factor is 3 divided by the hypotenuse.
    const rise = numberAt(tangent, i) / steep
    const fall = numberAt(tangent, i + 1) / steep
    const bulge = rise * rise + fall * fall

    if (bulge > 9) {
      const scale = 3 / Math.sqrt(bulge)
      tangent[i] = scale * rise * steep
      tangent[i + 1] = scale * fall * steep
    }
  }

  // Control points a third of the way along, which is where a cubic Bezier has to
  // put them to leave and arrive at exactly those tangents.
  let path = `M ${placed(first)}`

  for (const [index, point] of points.entries()) {
    const next = points[index + 1]
    if (!next) break

    const third = numberAt(gap, index) / 3
    const leave: [number, number] = [
      point[0] + third,
      point[1] + numberAt(tangent, index) * third,
    ]
    const arrive: [number, number] = [
      next[0] - third,
      next[1] - numberAt(tangent, index + 1) * third,
    ]

    path += ` C ${placed(leave)} ${placed(arrive)} ${placed(next)}`
  }

  return path
}

/**
 * Round numbers for the y-axis.
 *
 * An axis reading $10.00, $12.50, $15.00 looks like a machine leaked its
 * internals. Snapping the step to 1, 2, 2.5, 5 or 10 times a power of ten keeps
 * the labels on values a person would have picked.
 */
function niceTicks(lo: number, hi: number, count: number): number[] {
  const raw = (hi - lo) / count
  if (!Number.isFinite(raw) || raw <= 0) return [lo]

  const magnitude = 10 ** Math.floor(Math.log10(raw))
  const step =
    [1, 2, 2.5, 5, 10].map((multiple) => multiple * magnitude).find((s) => s >= raw) ??
    10 * magnitude

  const ticks: number[] = []
  for (let tick = Math.ceil(lo / step) * step; tick <= hi + 1e-9; tick += step) ticks.push(tick)
  return ticks
}

/**
 * Fit the axis to the readings rather than starting at zero.
 *
 * A balance moving between $6 and $12 drawn on a $0-$12 axis is a nearly flat
 * line, which hides exactly the movement worth seeing. Padding holds the
 * extremes off the edges. The cost is that a small wobble looks large, which is
 * why the axis labels stay visible: they give the real size of the movement.
 */
function fittedRange(values: number[]): { lo: number; hi: number } {
  const rawLo = Math.min(...values)
  const rawHi = Math.max(...values)
  // A flat series would otherwise divide by zero. Fall back to a visible band.
  const pad = (rawHi - rawLo) * 0.16 || Math.max(rawHi * 0.08, 0.5)
  return { lo: Math.max(0, rawLo - pad), hi: rawHi + pad }
}

/**
 * Only rows whose `basis` says they are a balance get plotted.
 *
 * A `usage` row holds spend so far, which climbs as money runs out. Drawing it
 * on the same axis as a balance would produce a chart that looks healthy while
 * the account empties, so those rows are excluded and counted instead.
 */
/**
 * Only rows that belong on this provider's line get plotted.
 *
 * Two kinds are dropped. A `usage` row holds spend so far, which climbs as
 * money runs out, so drawing it on a balance axis shows a healthy chart while
 * the account empties. A row from a different credential belongs to a different
 * account, and joining the two draws one line through two balances, which is
 * simply a lie about what happened.
 */
function balancePoints(series: Series[]): {
  series: Series
  points: SnapshotRow[]
  usageExcluded: number
  otherKeyExcluded: number
}[] {
  return series.map((entry) => {
    const usable = entry.points.filter((point) => point.basis !== 'usage')
    // Rows written before fingerprints existed cannot be attributed either way,
    // so they are kept rather than thrown away.
    const points = usable.filter(
      (point) =>
        point.keyFingerprint === null ||
        entry.keyFingerprint === null ||
        point.keyFingerprint === entry.keyFingerprint,
    )

    return {
      series: entry,
      points: points
        .slice()
        .sort((a, b) => parseUtc(a.recordedAt).getTime() - parseUtc(b.recordedAt).getTime()),
      usageExcluded: entry.points.length - usable.length,
      otherKeyExcluded: usable.length - points.length,
    }
  })
}

const timeOf = (point: SnapshotRow) => parseUtc(point.recordedAt).getTime()

/** The reading closest to `time`. Each provider is polled on its own clock. */
function nearest(points: SnapshotRow[], time: number): SnapshotRow | undefined {
  let best: SnapshotRow | undefined
  let bestDistance = Number.POSITIVE_INFINITY

  for (const point of points) {
    const distance = Math.abs(timeOf(point) - time)
    if (distance < bestDistance) {
      bestDistance = distance
      best = point
    }
  }

  return best
}

const shortDate = (time: number) =>
  new Date(time).toLocaleDateString(undefined, { day: 'numeric', month: 'short' })

/**
 * Axis labels have to suit the span they describe.
 *
 * A day's worth of readings all fall on one date, so five date labels read the
 * same and the axis says nothing. Under a couple of days the clock is the useful
 * thing; past that, the date is.
 */
function axisLabel(time: number, spanMs: number): string {
  const date = new Date(time)

  if (spanMs < 2 * 86_400_000) {
    return date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' })
  }

  return shortDate(time)
}

const readingTime = (value: string) =>
  parseUtc(value).toLocaleString(undefined, {
    day: 'numeric',
    month: 'short',
    hour: '2-digit',
    minute: '2-digit',
  })

export function BalanceChart({ series }: { series: Series[] }) {
  const [hoverTime, setHoverTime] = useState<number | null>(null)

  const plotted = balancePoints(series)
  const usageExcluded = plotted.reduce((total, entry) => total + entry.usageExcluded, 0)
  const otherKeyExcluded = plotted.reduce((total, entry) => total + entry.otherKeyExcluded, 0)
  const all = plotted.flatMap((entry) => entry.points)

  const legend = (
    <div className="flex flex-wrap items-center gap-4">
      {series.map((entry) => (
        <span key={entry.name} className="flex items-center gap-1.5">
          <span
            className="h-[6px] w-[6px] rounded-full"
            style={{ background: entry.hue }}
          />
          <span className="label-sm text-ink-dim">{entry.displayName}</span>
        </span>
      ))}
      {usageExcluded > 0 ? (
        <span className="label-sm text-ink-muted">
          {usageExcluded} usage reading{usageExcluded === 1 ? '' : 's'} excluded
        </span>
      ) : null}
      {/* Says why history looks short rather than leaving a gap unexplained. */}
      {otherKeyExcluded > 0 ? (
        <span className="label-sm text-ink-muted">
          {otherKeyExcluded} reading{otherKeyExcluded === 1 ? '' : 's'} from another key not shown
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

  const times = all.map(timeOf)
  const tMin = Math.min(...times)
  const tMax = Math.max(...times)
  const { lo, hi } = fittedRange(all.map((point) => point.remaining))
  const ticks = niceTicks(lo, hi, TICK_TARGET)

  // Time on the x-axis, not reading index: the poll interval is configurable and
  // refreshes can be manual, so the gaps between readings are not uniform.
  const x = (time: number) =>
    tMax === tMin ? WIDTH / 2 : ((time - tMin) / (tMax - tMin)) * WIDTH
  const y = (value: number) => HEIGHT - ((value - lo) / (hi - lo)) * HEIGHT

  const leftOf = (time: number) => `${((x(time) / WIDTH) * 100).toFixed(3)}%`
  const topOf = (value: number) => `${((y(value) / HEIGHT) * 100).toFixed(3)}%`

  const pathOf = (points: SnapshotRow[]) =>
    smoothPath(points.map((point) => [x(timeOf(point)), y(point.remaining)]))

  // Snap to a real reading rather than an arbitrary point on the line, so the
  // crosshair always sits on something that was actually measured.
  const anchor = hoverTime === null ? undefined : nearest(all, hoverTime)
  const marks: { entry: (typeof plotted)[number]; point: SnapshotRow }[] = []
  if (anchor) {
    for (const entry of plotted) {
      const point = nearest(entry.points, timeOf(anchor))
      if (point) marks.push({ entry, point })
    }
  }

  const tipLeft = anchor ? x(timeOf(anchor)) / WIDTH : 0
  // Past this point the panel would run off the right edge, so it swaps sides.
  const flip = tipLeft > 0.58
  // Clamped, not free: the panel is tall relative to the plot, so letting it
  // follow the cursor all the way to either edge would push it outside.
  const tipTop = anchor
    ? Math.min(68, Math.max(32, (y(anchor.remaining) / HEIGHT) * 100))
    : 50

  function onMove(event: React.MouseEvent<HTMLDivElement>) {
    const box = event.currentTarget.getBoundingClientRect()
    if (box.width === 0) return
    const fraction = (event.clientX - box.left) / box.width
    setHoverTime(tMin + fraction * (tMax - tMin))
  }

  return (
    <div>
      <div className="flex items-center justify-between">
        <span className="label text-ink-muted">Balance over time</span>
        {legend}
      </div>

      <div className="mt-3 flex" style={{ height: HEIGHT }}>
        {/* y-axis */}
        <div className="relative shrink-0" style={{ width: AXIS_WIDTH }}>
          {ticks.map((tick) => (
            <span
              key={tick}
              className="num pointer-events-none absolute right-2 -translate-y-1/2 text-[10px] text-ink-muted"
              style={{ top: topOf(tick) }}
            >
              {usd(tick)}
            </span>
          ))}
        </div>

        <div
          className="relative z-10 flex-1 cursor-crosshair select-none"
          onMouseMove={onMove}
          onMouseLeave={() => setHoverTime(null)}
        >
          <svg
            viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
            preserveAspectRatio="none"
            className="absolute inset-0 h-full w-full"
            role="img"
            aria-label="Remaining balance over time"
          >
            {/* Gridlines first, but nothing is filled over them any more. */}
            {ticks.map((tick) => (
              <line
                key={tick}
                x1="0"
                y1={y(tick)}
                x2={WIDTH}
                y2={y(tick)}
                stroke="var(--color-line)"
                strokeWidth="1"
                vectorEffect="non-scaling-stroke"
              />
            ))}

            {plotted.map((entry) =>
              entry.points.length < 2 ? null : (
                <path
                  key={entry.series.name}
                  d={pathOf(entry.points)}
                  fill="none"
                  stroke={entry.series.hue}
                  strokeWidth="2"
                  strokeLinejoin="round"
                  strokeLinecap="round"
                  vectorEffect="non-scaling-stroke"
                />
              ),
            )}
          </svg>

          {/* Current value at the end of each line, so the legend does not have
              to be matched against colours to read a number. */}
          {plotted.map((entry) => {
            const last = entry.points[entry.points.length - 1]
            if (!last || entry.points.length < 2) return null

            return (
              <span key={entry.series.name}>
                <span
                  className="pointer-events-none absolute h-[7px] w-[7px] -translate-x-1/2 -translate-y-1/2 rounded-full"
                  style={{
                    left: leftOf(timeOf(last)),
                    top: topOf(last.remaining),
                    background: entry.series.hue,
                    boxShadow: '0 0 0 3px var(--color-surface)',
                  }}
                />
                <span
                  className="num pointer-events-none absolute -translate-y-1/2 whitespace-nowrap pl-[11px] text-[10.5px]"
                  style={{
                    left: leftOf(timeOf(last)),
                    top: topOf(last.remaining),
                    color: entry.series.hue,
                    textShadow: HALO,
                  }}
                >
                  {usd(last.remaining)}
                </span>
              </span>
            )
          })}

          {anchor ? (
            <>
              <span
                className="pointer-events-none absolute top-0 bottom-0 w-px bg-line-strong"
                style={{ left: leftOf(timeOf(anchor)) }}
              />

              {marks.map(({ entry, point }) => (
                <span
                  key={entry.series.name}
                  className="pointer-events-none absolute h-2 w-2 -translate-x-1/2 -translate-y-1/2 rounded-full"
                  style={{
                    left: leftOf(timeOf(point)),
                    top: topOf(point.remaining),
                    background: entry.series.hue,
                    boxShadow: '0 0 0 3px var(--color-surface)',
                  }}
                />
              ))}

              <div
                className="pointer-events-none absolute z-10 min-w-[136px] rounded-[10px] border border-line-strong bg-panel px-2.5 py-2 shadow-[0_10px_26px_rgba(0,0,0,0.55)]"
                style={{
                  left: `${(tipLeft * 100).toFixed(3)}%`,
                  top: `${tipTop.toFixed(3)}%`,
                  transform: flip
                    ? 'translate(calc(-100% - 14px), -50%)'
                    : 'translate(14px, -50%)',
                }}
              >
                <div className="num text-[10px] text-ink-muted">
                  {readingTime(anchor.recordedAt)}
                </div>
                {marks.map(({ entry, point }) => (
                  <div key={entry.series.name} className="mt-1.5 flex items-center gap-2">
                    <span
                      className="h-1.5 w-1.5 shrink-0 rounded-full"
                      style={{ background: entry.series.hue }}
                    />
                    <span className="text-[10.5px] text-ink-dim">{entry.series.displayName}</span>
                    <span
                      className="num ml-auto pl-3 text-[11.5px]"
                      style={{ color: entry.series.hue }}
                    >
                      {usd(point.remaining)}
                    </span>
                  </div>
                ))}
                {marks.length > 1 ? (
                  <div className="num mt-1.5 border-t border-line pt-1.5 text-[10px] text-ink-muted">
                    {usd(marks.reduce((total, mark) => total + mark.point.remaining, 0))} combined
                  </div>
                ) : null}
              </div>
            </>
          ) : null}
        </div>

        {/* Gutter the end-of-line values overflow into. */}
        <div className="shrink-0" style={{ width: END_WIDTH }} />
      </div>

      <div
        className="num mt-2.5 flex justify-between text-[10px] text-ink-muted"
        style={{ paddingLeft: AXIS_WIDTH, paddingRight: END_WIDTH }}
      >
        {[0, 0.25, 0.5, 0.75, 1].map((fraction) => (
          <span key={fraction}>{axisLabel(tMin + fraction * (tMax - tMin), tMax - tMin)}</span>
        ))}
      </div>
    </div>
  )
}
