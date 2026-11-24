import {
  useCallback,
  useRef,
  useState,
  type KeyboardEvent,
  type PointerEvent as ReactPointerEvent,
} from "react";

/**
 * The draggable divider between chat and preview
 * (pockets-design-update-v3.md §8). Dragging resizes the preview column
 * live; the value is persisted only when the interaction ends (pointer up,
 * key press, double click) so a 60 Hz drag never writes to storage.
 *
 * Direction: dragging LEFT widens the preview (the divider moves left),
 * matching the keyboard rule `← = preview wider`. Home/End jump to the
 * minimum/maximum; a double click resets to the built-in ratio, reported as
 * `null`. The parent owns the clamped, effective width (`width`/`min`/`max`
 * come from the live window size, computed by App); this component only ever
 * reports widths inside that band.
 *
 * The track is the 1px border line itself, with an invisible 3px hit strip
 * either side (styles.css); the divider is pure DOM and behaves the same
 * outside Tauri.
 */
export function ColumnDivider({
  width,
  min,
  max,
  onResize,
  onCommit,
}: {
  /** Effective preview-column width in px. */
  width: number;
  min: number;
  max: number;
  /** Live width during a drag (a ratio reset arrives as `null`); nothing
      is persisted here. */
  onResize: (width: number | null) => void;
  /** The width when the interaction ended; persist it now. `null` means
      the built-in ratio. */
  onCommit: (width: number | null) => void;
}) {
  const [dragging, setDragging] = useState(false);
  // `last` is the newest width this drag reported; it is what a release
  // commits, so the commit never depends on the parent having re-rendered
  // with the live width first. A press that never moved commits nothing.
  const dragRef = useRef<{
    startX: number;
    startWidth: number;
    last: number | null;
  } | null>(null);

  const clamp = useCallback(
    (value: number) => Math.min(max, Math.max(min, Math.round(value))),
    [min, max],
  );

  const handlePointerDown = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      if (event.button !== 0) return;
      event.currentTarget.setPointerCapture(event.pointerId);
      dragRef.current = {
        startX: event.clientX,
        startWidth: width,
        last: null,
      };
      setDragging(true);
    },
    [width],
  );

  const handlePointerMove = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      const drag = dragRef.current;
      if (drag === null) return;
      // Dragging left grows the preview column.
      const next = clamp(drag.startWidth - (event.clientX - drag.startX));
      if (next === drag.last) return;
      drag.last = next;
      onResize(next);
    },
    [clamp, onResize],
  );

  const endDrag = useCallback(
    (event: ReactPointerEvent<HTMLDivElement>) => {
      const drag = dragRef.current;
      if (drag === null) return;
      dragRef.current = null;
      setDragging(false);
      if (event.currentTarget.hasPointerCapture(event.pointerId)) {
        event.currentTarget.releasePointerCapture(event.pointerId);
      }
      if (drag.last !== null) onCommit(drag.last);
    },
    [onCommit],
  );

  const handleKeyDown = useCallback(
    (event: KeyboardEvent<HTMLDivElement>) => {
      // Keyboard mirror of the drag: 16px steps, Home/End the clamp bounds.
      // Every key press is an interaction of its own, so it commits at once.
      let next: number | null = null;
      if (event.key === "ArrowLeft") next = clamp(width + 16);
      else if (event.key === "ArrowRight") next = clamp(width - 16);
      else if (event.key === "Home") next = min;
      else if (event.key === "End") next = max;
      else return;
      event.preventDefault();
      onResize(next);
      onCommit(next);
    },
    [clamp, width, min, max, onResize, onCommit],
  );

  const resetToRatio = useCallback(() => {
    onResize(null);
    onCommit(null);
  }, [onResize, onCommit]);

  return (
    <div
      className="column-divider"
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize preview"
      aria-valuenow={width}
      aria-valuemin={min}
      aria-valuemax={max}
      tabIndex={0}
      data-dragging={dragging ? "true" : undefined}
      onPointerDown={handlePointerDown}
      onPointerMove={handlePointerMove}
      onPointerUp={endDrag}
      onPointerCancel={endDrag}
      onKeyDown={handleKeyDown}
      onDoubleClick={resetToRatio}
    />
  );
}
