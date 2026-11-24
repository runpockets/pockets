import { createContext, useContext } from "react";

/**
 * Whether a UI overlay is currently open
 * (pockets-design-update-v3.md §5). The native preview pane renders above
 * the entire window — outside the webview's DOM, above every CSS z-index —
 * so while any overlay is open (the All-pockets page's dialogs, a
 * confirmation, a menu) the pane must be hidden, or the dialog would open
 * under live web content.
 *
 * The provider lives in App and is the single owner of hide/restore for
 * overlays: it hides the pane while any overlay is open and re-shows it
 * afterwards by re-reporting the same bounds it was given (same content
 * session, same URL). Only the workspace needs it — the chat-header menu is
 * the one popover in the shell that can reach the preview rect, and the
 * dialogs on the two catalog pages cannot, because the workspace (and with
 * it the pane's session) is unmounted there. The preview column's own
 * bind/hide lifecycle (usePreviewPane) stays separate and re-asserts the
 * pane when a session binds or bounds change; overlay restore happens
 * through that same report path, so one code path re-shows the pane in every
 * case.
 */
export interface OverlayApi {
  /** Open an overlay: hides the native preview pane, once for a stack of
      open overlays. Pass `false` to close — restores the pane. */
  setOverlay: (open: boolean) => void;
}

const OverlayContext = createContext<OverlayApi>({ setOverlay: () => {} });

export const OverlayProvider = OverlayContext.Provider;

/** Returns the stable `{ setOverlay }`. Outside a provider (component tests
    that mount a page alone) it is a no-op, exactly like the real thing in a
    browser without a native pane. */
export function useOverlay(): OverlayApi {
  return useContext(OverlayContext);
}
