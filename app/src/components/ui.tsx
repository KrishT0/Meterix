import { useEffect, useState } from 'react'
import type { ButtonHTMLAttributes, ReactNode } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'

import type { ProviderOverview, RefreshOutcome } from '../lib/api'

export type Tone = 'teal' | 'amber' | 'copper' | 'muted'

interface ToneStyle {
  dot: string
  pill: string
}

/**
 * Status, and only status.
 *
 * Which provider something belongs to is a separate axis — see lib/palette.ts —
 * so nothing here identifies a provider, and nothing there reports a problem.
 * The card tint and the icon box used to live here, which is what made the two
 * axes impossible to keep apart: the tint was the health signal, so it could not
 * also say which provider a panel belonged to.
 *
 * Every class name is spelled out in full because Tailwind scans source text;
 * a built-up name like `bg-${tone}` would never make it into the stylesheet.
 */
export const tones: Record<Tone, ToneStyle> = {
  teal: { dot: 'bg-teal', pill: 'border-teal-dim/50 text-teal' },
  amber: { dot: 'bg-amber', pill: 'border-amber-line text-amber-dim' },
  copper: { dot: 'bg-copper', pill: 'border-copper-dim/40 text-copper' },
  muted: { dot: 'bg-ink-muted', pill: 'border-line-strong text-ink-dim' },
}

export type Health = 'ok' | 'low' | 'error' | 'unknown'

/**
 * Health from the balance against the provider's own threshold.
 *
 * The threshold arrives with the provider rather than living here, so the tray
 * and the dashboard cannot disagree about what counts as low.
 */
export function healthOf(
  provider: ProviderOverview,
  outcome: RefreshOutcome | undefined,
): Health {
  // Nothing configured cannot be in error: there was no check to fail. The
  // provider with no key fails with `missing_credential`, which is the state a
  // fresh install is in, so testing the outcome first painted the header pill
  // an error colour while it read "0 tracked".
  if (!provider.configured) return 'unknown'
  if (outcome && !outcome.ok) return 'error'
  // A usage figure is not a balance, so it cannot be judged against one.
  if (provider.basis === 'usage' || provider.balance === null) return 'unknown'
  return provider.balance < provider.threshold ? 'low' : 'ok'
}

export function toneOf(health: Health): Tone {
  switch (health) {
    case 'ok':
      return 'teal'
    case 'low':
      return 'amber'
    case 'error':
      return 'copper'
    case 'unknown':
      return 'muted'
  }
}

export function Label({ children, className = '' }: { children: ReactNode; className?: string }) {
  return <span className={`label ${className}`}>{children}</span>
}

export function Pill({ children, className = '' }: { children: ReactNode; className?: string }) {
  return (
    <span className={`label-sm rounded-full border px-2 py-0.5 ${className}`}>{children}</span>
  )
}

export function StatusDot({ tone }: { tone: Tone }) {
  return <span className={`h-[7px] w-[7px] shrink-0 rounded-full ${tones[tone].dot}`} />
}

type ButtonProps = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: 'primary' | 'secondary' | 'ghost'
}

const buttonVariants = {
  primary: 'bg-amber font-semibold text-[#1A1408] hover:brightness-110',
  secondary: 'border border-line-strong font-medium text-ink hover:border-ink-muted',
  ghost: 'font-medium text-ink-dim hover:text-ink',
}

export function Button({ variant = 'secondary', className = '', ...rest }: ButtonProps) {
  return (
    <button
      className={`rounded-lg px-3 py-1.5 text-[12px] transition disabled:cursor-not-allowed disabled:opacity-40 ${buttonVariants[variant]} ${className}`}
      {...rest}
    />
  )
}

// ---------------------------------------------------------------------------
// Window controls
// ---------------------------------------------------------------------------

/* The window has no native title bar, so these are its controls. Three rules
 * shape them, and each is load bearing:
 *
 * - They sit outside the drag region. A clickable element already blocks dragging,
 *   and these carry `data-tauri-drag-region="false"` so the intent is written down
 *   rather than inherited from an implementation detail.
 * - There is no double-click handler. Tauri's own drag-region script already
 *   toggles maximise on a double click, so adding one here would toggle twice and
 *   cancel itself out.
 * - Only the close control is coloured, and only on hover. Three coloured glyphs in
 *   a row would each be competing to be the primary action. */

type WindowControl = 'minimise' | 'maximise' | 'close'

const controlPaths: Record<WindowControl, ReactNode> = {
  // The bar is drawn on the same line the other two centre on. It was at y=6, which
  // is above centre in a 16-unit box, so the minimise icon sat a little higher than
  // the close beside it. The coordinates came from the 12-unit mockup.
  minimise: <path d="M3 8h10" />,
  maximise: <rect x="3.5" y="3.5" width="9" height="9" rx="1.5" />,
  close: <path d="M4 4l8 8M12 4l-8 8" />,
}

/** The controls a window needs, and nothing else. */
export function WindowControls({ window: which }: { window: 'dashboard' | 'popover' }) {
  const [maximised, setMaximised] = useState(false)
  // Held here rather than asked for with `window.confirm`, which is not a dialog
  // in this webview: nothing appeared, the call answered no, and quitting became a
  // button that did nothing. The dashboard had already learned this once for
  // removing a key and this one was left behind.
  const [askingToQuit, setAskingToQuit] = useState(false)

  useEffect(() => {
    if (which !== 'dashboard') return

    const current = getCurrentWindow()
    let stop: (() => void) | undefined

    void current.isMaximized().then(setMaximised)
    // Kept in step with the window rather than with the click: it can also be
    // maximised by double-clicking the drag region, or by the window manager.
    void current
      .onResized(() => void current.isMaximized().then(setMaximised))
      .then((unlisten) => {
        stop = unlisten
      })

    return () => stop?.()
  }, [which])

  const controls: WindowControl[] =
    // The popover has nothing to maximise to and no taskbar entry, so its minimise
    // takes the window away instead of putting it in the taskbar. It asks nothing:
    // hiding a popover is what losing focus already does, and it is not quitting.
    which === 'dashboard' ? ['minimise', 'maximise', 'close'] : ['minimise', 'close']

  return (
    <>
      <div className="ml-1 flex items-center gap-0.5 border-l border-line pl-2.5">
      {controls.map((control) => {
        // The popover's close ends the app, so it is named for that rather than for
        // the shape of the icon.
        const label =
          control === 'close' && which === 'popover'
            ? 'Quit Meterix'
            : control === 'maximise' && maximised
              ? 'Restore'
              : control

        return (
          <button
            key={control}
            type="button"
            data-tauri-drag-region="false"
            onClick={() => {
              const current = getCurrentWindow()

              if (control === 'minimise') {
                // A popover with no taskbar entry has nothing to minimise to.
                void (which === 'popover' ? current.hide() : current.minimize())
              } else if (control === 'maximise') {
                void current.toggleMaximize()
              } else if (which === 'popover') {
                // Quitting stops the watching, so it is the one control worth a
                // question, and this is the only window that has it.
                setAskingToQuit(true)
              } else {
                // Closing the window hides it: the tray keeps checking, which is what
                // the settings screen promises. Nothing is lost, so nothing is asked.
                void current.close()
              }
            }}
            aria-label={label}
            title={label}
            className={`flex h-7 w-8 items-center justify-center rounded-[7px] text-ink-dim transition hover:bg-panel hover:text-ink ${
              control === 'close' ? 'hover:bg-copper-tint hover:text-copper' : ''
            }`}
          >
            <svg
              width="12"
              height="12"
              viewBox="0 0 16 16"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.4"
              strokeLinecap="round"
            >
              {controlPaths[control]}
            </svg>
          </button>
        )
      })}
      </div>

      {/* Quitting stops the watching, so it is worth one question. A stray click on
          the only control that ends the app says nothing about wanting it ended. */}
      {askingToQuit ? (
        <ConfirmDialog
          title="Quit Meterix?"
          detail="Balances stop being checked until you open it again. Readings already stored are kept."
          confirmLabel="Quit"
          cancelLabel="Keep running"
          onConfirm={() => void invoke('quit_app')}
          onCancel={() => setAskingToQuit(false)}
        />
      ) : null}
    </>
  )
}

// ---------------------------------------------------------------------------
// Resize edges
// ---------------------------------------------------------------------------

/* An undecorated window can lose the draggable border a native frame gives you,
 * and losing it would be a worse trade than a bar that does not match the design.
 * This puts the edges back.
 *
 * It starts 4px outside the viewport because the outermost pixels of a window can
 * belong to the resize border rather than the page, so a strip drawn inside would
 * be a target that sometimes is not hit.
 *
 * ponytail: mounted only when the window has actually lost its frame, so it costs
 * nothing while the native bar is still there. Deleting this component and its one
 * render is the whole revert. */

type Edge = 'n' | 's' | 'e' | 'w' | 'ne' | 'nw' | 'se' | 'sw'

/** Mirrors the package's own union, which it declares but does not export.
 *  Identical members, so it satisfies the method's parameter structurally. */
type ResizeDirection =
  | 'East'
  | 'North'
  | 'NorthEast'
  | 'NorthWest'
  | 'South'
  | 'SouthEast'
  | 'SouthWest'
  | 'West'

const edgeDirections: Record<Edge, ResizeDirection> = {
  n: 'North',
  s: 'South',
  e: 'East',
  w: 'West',
  ne: 'NorthEast',
  nw: 'NorthWest',
  se: 'SouthEast',
  sw: 'SouthWest',
}

/* Corners before edges, so the corners win the overlap in source order. */
const edgeStyles: Record<Edge, string> = {
  nw: 'left-0 top-0 h-3 w-3 cursor-nwse-resize',
  ne: 'right-0 top-0 h-3 w-3 cursor-nesw-resize',
  sw: 'bottom-0 left-0 h-3 w-3 cursor-nesw-resize',
  se: 'bottom-0 right-0 h-3 w-3 cursor-nwse-resize',
  n: 'left-3 right-3 top-0 h-1.5 cursor-ns-resize',
  s: 'bottom-0 left-3 right-3 h-1.5 cursor-ns-resize',
  w: 'bottom-3 left-0 top-3 w-1.5 cursor-ew-resize',
  e: 'bottom-3 right-0 top-3 w-1.5 cursor-ew-resize',
}

export function ResizeEdges() {
  const [needed, setNeeded] = useState(false)

  useEffect(() => {
    let live = true

    // Asked of the window rather than assumed from config, so the overlay cannot
    // disagree with the window about what it looks like.
    void getCurrentWindow()
      .isDecorated()
      .then((decorated) => {
        if (live && !decorated) setNeeded(true)
      })

    return () => {
      live = false
    }
  }, [])

  if (!needed) return null

  return (
    <div className="pointer-events-none fixed inset-[-4px] z-40">
      {(Object.keys(edgeDirections) as Edge[]).map((edge) => (
        <div
          key={edge}
          data-tauri-drag-region="false"
          onMouseDown={(event) => {
            event.preventDefault()
            void getCurrentWindow().startResizeDragging(edgeDirections[edge])
          }}
          className={`pointer-events-auto absolute ${edgeStyles[edge]}`}
        />
      ))}
    </div>
  )
}

function icon(path: ReactNode, size = 12) {
  return function Icon({ className = '' }: { className?: string }) {
    return (
      <svg
        width={size}
        height={size}
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth={2.2}
        strokeLinecap="round"
        strokeLinejoin="round"
        className={className}
        aria-hidden="true"
      >
        {path}
      </svg>
    )
  }
}

export const CheckIcon = icon(<path d="M20 6 9 17l-5-5" />, 10)
export const CrossIcon = icon(<path d="M18 6 6 18M6 6l12 12" />, 10)
export const WarningIcon = icon(<path d="M12 8v5M12 17h.01" />, 10)
export const InfoIcon = icon(
  <>
    <circle cx="12" cy="12" r="9" />
    <path d="M12 16v-4M12 8h.01" />
  </>,
)
export const PencilIcon = icon(
  <path d="M12 20h9M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z" />,
)
export const RefreshIcon = icon(
  <>
    <path d="M21 12a9 9 0 1 1-3-6.7" />
    <path d="M21 4v5h-5" />
  </>,
  12,
)

/**
 * A small yes/no modal, in place of `window.confirm`.
 *
 * The browser dialog cannot be styled, blocks the whole webview, and looks like
 * a page error rather than something the app is asking. This one is a proper
 * choice: Escape or clicking away cancels, and focus starts on Cancel so the
 * destructive option is never the one under the return key.
 */
export function ConfirmDialog({
  title,
  detail,
  confirmLabel = 'Yes',
  cancelLabel = 'No',
  busy = false,
  onConfirm,
  onCancel,
}: {
  title: string
  detail?: string
  confirmLabel?: string
  cancelLabel?: string
  busy?: boolean
  onConfirm: () => void
  onCancel: () => void
}) {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onCancel()
    }

    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onCancel])

  return (
    <div
      className="fixed inset-0 z-50 grid place-items-center bg-backdrop/70 p-6"
      role="presentation"
      onClick={onCancel}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label={title}
        className="w-full max-w-[380px] rounded-[10px] border border-line-strong bg-panel p-4 shadow-2xl"
        onClick={(event) => event.stopPropagation()}
      >
        <p className="text-[13px] font-medium">{title}</p>
        {detail ? (
          <p className="num mt-1.5 text-[11px] leading-relaxed text-ink-muted">{detail}</p>
        ) : null}

        <div className="mt-4 flex justify-end gap-2">
          <Button variant="secondary" autoFocus onClick={onCancel}>
            {cancelLabel}
          </Button>
          <Button variant="primary" disabled={busy} onClick={onConfirm}>
            {confirmLabel}
          </Button>
        </div>
      </div>
    </div>
  )
}
