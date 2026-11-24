/**
 * Shared status primitives (pockets-design-update-v2.md §4): the 7×7px
 * square pip that always carries a capitalized word next to it, the two
 * allowed animations (spinner, "working" dots) and the segmented control
 * that replaces v1's `[bracketed]` tabs. All square geometry; all styling
 * lives in styles.css against the token set.
 */

import type { ReactNode } from "react";

/* ------------------------------------------------------------------ pip */

/** Running fills `--ok`; Building and Failed fill `--warn`; Stopped is a
    1px `--text-faint` outline with no fill. Never color alone: the word
    travels with the pip. */
export type PipTone = "ok" | "warn" | "stopped";

export function StatusPip({ tone, word }: { tone: PipTone; word: string }) {
  return (
    <span className="status-line-item">
      <span className="status-pip" data-tone={tone} aria-hidden="true" />
      {word}
    </span>
  );
}

/* ----------------------------------------------------------- animations */

/** 10px square, 2px `--warn` border with the right side transparent,
    1.2s linear rotation; static under prefers-reduced-motion (CSS). */
export function Spinner({ className }: { className?: string }) {
  return (
    <span
      className={className ? `spinner ${className}` : "spinner"}
      aria-hidden="true"
    />
  );
}

/** Three 6px `--text-dim` squares, opacity pulse 0.25→1, 1.2s, staggered
    0.2s; static under prefers-reduced-motion (CSS). */
export function WorkingDots({ label = "Working" }: { label?: string }) {
  return (
    <span className="working-dots" role="status">
      <span className="working-dot" aria-hidden="true" />
      <span className="working-dot" aria-hidden="true" />
      <span className="working-dot" aria-hidden="true" />
      <span className="working-dots-label">{label}</span>
    </span>
  );
}

/* ------------------------------------------------------------- segmented */

export interface SegmentedItem {
  id: string;
  label: string;
  /** Optional count rendered inside the segment in `--text-dim`. */
  count?: number;
}

/**
 * The v2 segmented control (§7/§8): outer 1px border, 2px padding, 2px
 * gap; active segment `--segment-active` + `--text`, inactive transparent +
 * `--text-dim`. Keeps the tablist semantics v1 established, including
 * arrow-key / Home / End navigation.
 */
export function Segmented({
  items,
  value,
  onChange,
  ariaLabel,
  className,
}: {
  items: SegmentedItem[];
  value: string;
  onChange: (id: string) => void;
  ariaLabel: string;
  className?: string;
}) {
  const onKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const index = items.findIndex((item) => item.id === value);
    let next: number | null = null;
    if (event.key === "ArrowRight") next = index + 1;
    else if (event.key === "ArrowLeft") next = index - 1;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = items.length - 1;
    if (next === null) return;
    event.preventDefault();
    const target = items[(next + items.length) % items.length];
    if (target !== undefined) {
      onChange(target.id);
      const node = event.currentTarget.querySelector<HTMLElement>(
        `[data-segment-id="${CSS.escape(target.id)}"]`,
      );
      node?.focus();
    }
  };

  return (
    <div
      className={className ? `segmented ${className}` : "segmented"}
      role="tablist"
      aria-label={ariaLabel}
      onKeyDown={onKeyDown}
    >
      {items.map((item) => {
        const active = item.id === value;
        return (
          <button
            key={item.id}
            type="button"
            role="tab"
            data-segment-id={item.id}
            aria-selected={active}
            tabIndex={active ? 0 : -1}
            className="segmented-item"
            onClick={() => onChange(item.id)}
          >
            {item.label}
            {item.count !== undefined && (
              <span className="segmented-count">{item.count}</span>
            )}
          </button>
        );
      })}
    </div>
  );
}

/**
 * A quiet square icon button (34×34 in toolbars): borderless, with the
 * global hover tint and — for a toggle — an `aria-pressed` fill. `label` is
 * the accessible name; the icon alone is never the only signal.
 */
export function IconButton({
  label,
  onClick,
  disabled,
  children,
  className,
  title,
  pressed,
}: {
  label: string;
  onClick?: () => void;
  disabled?: boolean;
  children: ReactNode;
  className?: string;
  title?: string;
  /** Toggle-button state (header collapse controls); omitted for plain actions. */
  pressed?: boolean;
}) {
  return (
    <button
      type="button"
      className={className ? `icon-button ${className}` : "icon-button"}
      aria-label={label}
      title={title ?? label}
      aria-pressed={pressed}
      onClick={onClick}
      disabled={disabled}
    >
      {children}
    </button>
  );
}
