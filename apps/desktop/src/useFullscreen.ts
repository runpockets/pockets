import { createContext, useContext } from "react";

/**
 * Browser full-screen state hook. In the cross-platform web dashboard,
 * standard browser chrome handles window controls so no macOS traffic-light
 * inset adjustment is needed.
 */
export function useFullscreen(): boolean {
  return false;
}

const FullscreenContext = createContext(false);

/** Provided once by App with the value of `useFullscreen()`. */
export const FullscreenProvider = FullscreenContext.Provider;

/** Returns whether native full-screen inset adjustments are active. */
export function useIsFullscreen(): boolean {
  return useContext(FullscreenContext);
}
