import { useEffect, useRef, useState } from 'react'

import type { ProviderOverview } from '../lib/api'

/**
 * Which provider a pasted key will be saved to.
 *
 * A name rather than a monogram: "OR" is the English word "or", and it would
 * sit inside a control that answers "save this key to ___". Identity should not
 * have to be learned.
 */
export function ProviderPicker({
  providers,
  value,
  onChange,
}: {
  providers: ProviderOverview[]
  value: string
  onChange: (name: string) => void
}) {
  const [open, setOpen] = useState(false)
  const root = useRef<HTMLDivElement>(null)

  // Close when the click lands anywhere else, or on Escape.
  useEffect(() => {
    if (!open) return

    const onPointerDown = (event: PointerEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false)
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setOpen(false)
    }

    document.addEventListener('pointerdown', onPointerDown)
    document.addEventListener('keydown', onKeyDown)

    return () => {
      document.removeEventListener('pointerdown', onPointerDown)
      document.removeEventListener('keydown', onKeyDown)
    }
  }, [open])

  const selected = providers.find((provider) => provider.name === value)

  return (
    <div ref={root} className="relative">
      <button
        type="button"
        onClick={() => setOpen((wasOpen) => !wasOpen)}
        aria-haspopup="menu"
        aria-expanded={open}
        className={`flex items-center gap-2 rounded-md border bg-surface px-2.5 py-1.5 transition ${
          open ? 'border-amber' : 'border-line-strong hover:border-ink-muted'
        }`}
      >
        <span className={`text-[12px] font-medium ${open ? 'text-amber' : 'text-ink'}`}>
          {selected?.displayName ?? value}
        </span>
        <svg
          width="9"
          height="9"
          viewBox="0 0 24 24"
          fill="none"
          stroke={open ? '#E0A64B' : '#7A7871'}
          strokeWidth="3"
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden="true"
        >
          <path d="m6 9 6 6 6-6" />
        </svg>
      </button>

      {open ? (
        <div
          role="menu"
          aria-label="Save this key to"
          className="absolute top-[calc(100%+6px)] right-0 z-20 w-[300px] overflow-hidden rounded-[10px] border border-line-strong bg-panel shadow-[0_18px_40px_-12px_rgba(0,0,0,0.85)]"
        >
          <div className="border-b border-line px-3.5 py-2">
            <span className="label-sm text-ink-muted">Save this key to</span>
          </div>

          {providers.map((provider) => {
            const isSelected = provider.name === value

            return (
              <button
                key={provider.name}
                type="button"
                role="menuitemradio"
                aria-checked={isSelected}
                onClick={() => {
                  onChange(provider.name)
                  setOpen(false)
                }}
                className={`flex w-full items-center gap-3 border-l-2 px-3.5 py-2.5 text-left transition ${
                  isSelected ? 'border-amber bg-inset' : 'border-transparent hover:bg-line/25'
                }`}
              >
                <span className="text-[13px] font-medium">{provider.displayName}</span>
                <span
                  className={`num ml-auto text-[11px] ${
                    provider.configured ? 'text-teal' : 'text-ink-muted'
                  }`}
                >
                  {provider.configured ? 'key stored' : 'no key'}
                </span>
              </button>
            )
          })}

          <div className="border-t border-line bg-inset px-3.5 py-2">
            <span className="num text-[11px] text-ink-muted">
              Choosing a provider only picks the slot. Nothing is written until you save.
            </span>
          </div>
        </div>
      ) : null}
    </div>
  )
}
