/**
 * Which shade each provider takes, by position.
 *
 * The ten hues live as `--color-hue-1` … `--color-hue-10` in index.css. This maps
 * a provider's position in the list to one of them.
 *
 * Interleaved through the spectrum rather than taken left to right. Shipping with
 * two providers means positions 0 and 1 are the whole product, and taking the
 * shades in hue order handed those two the closest pair in the set — Lime and
 * Green, dE 18.4. Interleaved, the same pair is dE 100.3, and the first four stay
 * above dE 34.
 *
 * Identity, not status. These say which provider something is, and never that
 * something is wrong: amber, orange and copper already mean "under your
 * threshold" and "a key was rejected", so a provider whose line happened to be
 * amber would be unreadable. See index.css for how the set was chosen.
 */
const ORDER = [1, 7, 4, 10, 2, 8, 5, 9, 3, 6]

/**
 * The colour value for the provider at this position.
 *
 * Assign it to the `--hue` custom property, and the `hue-*` utilities in
 * index.css derive the tint, the border, the dot and the label colour from it.
 *
 * Past ten providers the colours repeat in order, which is visible and honest,
 * unlike quietly running out of colour.
 */
export function hueAt(index: number): string {
  return `var(--color-hue-${ORDER[index % ORDER.length]})`
}
