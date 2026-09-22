import type { ButtonHTMLAttributes, ReactNode } from 'react'

import type { ProviderOverview, RefreshOutcome } from '../lib/api'

export type Tone = 'teal' | 'amber' | 'copper' | 'muted'

interface ToneStyle {
  dot: string
  card: string
  pill: string
  text: string
  iconBox: string
}

/**
 * Every class name is spelled out in full because Tailwind scans source text;
 * a built-up name like `bg-${tone}` would never make it into the stylesheet.
 */
export const tones: Record<Tone, ToneStyle> = {
  teal: {
    dot: 'bg-teal',
    card: 'border-teal-dim/40 bg-teal-tint',
    pill: 'border-teal-dim/50 text-teal',
    text: 'text-teal',
    iconBox: 'bg-teal',
  },
  amber: {
    dot: 'bg-amber',
    card: 'border-amber-line bg-amber-tint',
    pill: 'border-amber-line text-amber-dim',
    text: 'text-amber',
    iconBox: 'bg-amber',
  },
  copper: {
    dot: 'bg-copper',
    card: 'border-copper-dim/40 bg-copper-tint',
    pill: 'border-copper-dim/40 text-copper',
    text: 'text-copper',
    iconBox: 'bg-copper',
  },
  muted: {
    dot: 'bg-ink-muted',
    card: 'border-line bg-panel',
    pill: 'border-line-strong text-ink-dim',
    text: 'text-ink-muted',
    iconBox: 'bg-line-strong',
  },
}

export type Health = 'ok' | 'low' | 'error' | 'unknown'

/**
 * Thresholds belong per provider, which needs a schema change. Until then one
 * default stands in for all of them.
 */
export const LOW_BALANCE_THRESHOLD = 2

export function healthOf(
  provider: ProviderOverview,
  outcome: RefreshOutcome | undefined,
): Health {
  if (outcome && !outcome.ok) return 'error'
  if (!provider.configured) return 'unknown'
  // A usage figure is not a balance, so it cannot be judged against one.
  if (provider.basis === 'usage' || provider.balance === null) return 'unknown'
  return provider.balance < LOW_BALANCE_THRESHOLD ? 'low' : 'ok'
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
