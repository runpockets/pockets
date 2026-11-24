import { useCallback, type MouseEventHandler } from "react";

/**
 * Title-bar mouse-down hook. In the browser-hosted Pockets dashboard, the
 * host browser/PWA window owns native dragging, so this is a lightweight no-op.
 */
export function useTitlebarDrag(): MouseEventHandler<HTMLElement> {
  return useCallback(() => {}, []);
}
