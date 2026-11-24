import type { ReactNode } from "react";
import { useTitlebarDrag } from "./useTitlebarDrag";
import { useIsFullscreen } from "./useFullscreen";
import "./TitleBar.css";

/**
 * One column header in the unified 52px title row
 * (pockets-design-update-v3.md §1.4). Every routed page mounts exactly one
 * per visible column — sidebar, main (chat or management page), preview —
 * with the same height and the same bottom border, so the borders read as
 * one continuous line at the same y. Slots: `leading` controls, a `title`
 * that truncates, `trailing` controls pinned right.
 *
 * `reserveTrafficLights` marks the header on the column currently at the
 * window's left edge. The padding is added only inside Tauri (outside, no
 * buttons overlay the row) and never in full screen (§1.2: the lights move
 * into the menu bar), so the rule lives here rather than at every call site.
 *
 * The whole row drags the window through `useTitlebarDrag`; real controls
 * inside the slots are exempt on their own account.
 */
export function TitleBar({
  ariaLabel,
  reserveTrafficLights = false,
  leading,
  title,
  trailing,
  className,
}: {
  /** Toolbar name for assistive tech: "Sidebar", "Chat", "Preview"… */
  ariaLabel: string;
  /** True only for the leftmost visible column's header. */
  reserveTrafficLights?: boolean;
  leading?: ReactNode;
  title?: ReactNode;
  trailing?: ReactNode;
  /** Extra class for column-specific styling (e.g. the sidebar's `--chrome`
      background — the column backgrounds stay as they were in v2). */
  className?: string;
}) {
  const onMouseDown = useTitlebarDrag();
  const fullscreen = useIsFullscreen();
  const reserve =
    reserveTrafficLights && !fullscreen && "__TAURI_INTERNALS__" in window;
  return (
    <div
      className={className ? `titlebar ${className}` : "titlebar"}
      role="toolbar"
      aria-label={ariaLabel}
      data-reserve-lights={reserve ? "true" : undefined}
      onMouseDown={onMouseDown}
    >
      {leading !== undefined && (
        <div className="titlebar-leading">{leading}</div>
      )}
      {title !== undefined && <div className="titlebar-title">{title}</div>}
      {trailing !== undefined && (
        <div className="titlebar-trailing">{trailing}</div>
      )}
    </div>
  );
}
