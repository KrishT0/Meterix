/**
 * Which shade identifies a provider.
 *
 * Identity and status are separate axes on purpose. These ten say "this is which
 * provider" and are used for the chart line, the dot, the meter, the basis label
 * and the card's tint and border. They deliberately avoid amber, orange and
 * copper, because those already mean "under your threshold" and "a key was
 * rejected" — a provider whose line happened to be amber would be unreadable.
 *
 * The index is its position in the list, so a provider keeps its colour as long
 * as the list order holds. Reordering the list recolours every provider in every
 * surface at once, which is why nothing else stores an index.
 *
 * There are ten and no more. Beyond that, two providers sharing a shade with a
 * distinguishing dash, or filtering the chart, beats an eleventh hue nobody can
 * name.
 */
export const HUES = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10] as const

export type Hue = (typeof HUES)[number]

/**
 * The colour value for a provider at this position in the list.
 *
 * Assign it to the `--hue` custom property on an element, and the `hue-*`
 * utilities in index.css derive the tint, the border, the dot and the label
 * colour from that one value.
 */
export function hueAt(index: number): string {
  // Wrapped rather than clamped: past ten providers the colours repeat in order,
  // which is visible and honest, unlike quietly running out of colour.
  return `var(--color-hue-${HUES[index % HUES.length]})`
}
