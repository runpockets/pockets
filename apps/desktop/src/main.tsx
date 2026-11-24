import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import App from "./App";
import { applyTheme, loadThemeMode } from "./theme";
import "./fonts.css";
import "./tokens.css";
import "./styles.css";

/* Set data-theme before the first paint so neither theme flashes
   unstyled (pockets-design-update-v2.md §9). */
applyTheme(loadThemeMode());

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
