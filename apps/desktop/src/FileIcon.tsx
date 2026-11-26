import "./FileIcon.css";

export type FileIconKind =
  | "container"
  | "test"
  | "js"
  | "ts"
  | "css"
  | "html"
  | "json"
  | "yaml"
  | "markdown"
  | "image"
  | "shell"
  | "config"
  | "generic";

export function fileIconKind(path: string): FileIconKind {
  const slash = path.lastIndexOf("/");
  const base = (slash === -1 ? path : path.slice(slash + 1)).toLowerCase();

  if (
    base === "containerfile" ||
    base === "dockerfile" ||
    base.startsWith("containerfile.") ||
    base.startsWith("dockerfile.")
  ) {
    return "container";
  }

  if (
    /\.(test|spec)\.[a-z0-9]+$/.test(base) ||
    base.endsWith(".check.cjs") ||
    base.endsWith(".check.js")
  ) {
    return "test";
  }

  if (base.startsWith(".") || base === "license" || base === "license.txt") {
    return "config";
  }

  const dot = base.lastIndexOf(".");
  const ext = dot === -1 ? "" : base.slice(dot + 1);

  switch (ext) {
    case "js":
    case "mjs":
    case "cjs":
    case "jsx":
      return "js";
    case "ts":
    case "mts":
    case "cts":
    case "tsx":
      return "ts";
    case "css":
    case "scss":
    case "less":
      return "css";
    case "html":
    case "htm":
      return "html";
    case "json":
    case "jsonc":
      return "json";
    case "yaml":
    case "yml":
    case "toml":
      return "yaml";
    case "md":
    case "mdx":
      return "markdown";
    case "svg":
    case "png":
    case "jpg":
    case "jpeg":
    case "gif":
    case "webp":
    case "ico":
      return "image";
    case "sh":
    case "bash":
    case "zsh":
      return "shell";
    default:
      return "generic";
  }
}

/**
 * Minimal VS Code-style file-type icon (14px inline SVG, themed through
 * tokens.css custom properties). Always decorative next to a file path.
 */
export function FileIcon({ path, size = 14 }: { path: string; size?: number }) {
  const kind = fileIconKind(path);
  const common = {
    className: `file-icon file-icon--${kind}`,
    width: size,
    height: size,
    viewBox: "0 0 16 16",
    fill: "none",
    stroke: "currentColor",
    strokeWidth: 1.4,
    strokeLinecap: "round" as const,
    strokeLinejoin: "round" as const,
    "aria-hidden": true,
    focusable: false as const,
  };

  switch (kind) {
    case "container":
      return (
        <svg {...common}>
          <path d="M8 1.8 13.5 4.8v6.4L8 14.2 2.5 11.2V4.8L8 1.8Z" />
          <path d="M2.5 4.8 8 7.8l5.5-3M8 7.8v6.4" />
        </svg>
      );
    case "test":
      return (
        <svg {...common}>
          <path d="M6 2h4M7 2v3.6L3.4 12a1.3 1.3 0 0 0 1.1 2h7a1.3 1.3 0 0 0 1.1-2L9 5.6V2" />
          <path d="M5 10.5h6" />
        </svg>
      );
    case "js":
      return (
        <svg {...common}>
          <rect x="2" y="2" width="12" height="12" rx="1.5" />
          <path d="M6.4 6.2v3.9a1.1 1.1 0 0 1-1.6.9" />
          <path d="M11.1 7.1a1.1 1.1 0 0 0-1.9.7c0 .9 2 .8 2 1.9a1.2 1.2 0 0 1-2.1.7" />
        </svg>
      );
    case "ts":
      return (
        <svg {...common}>
          <rect x="2" y="2" width="12" height="12" rx="1.5" />
          <path d="M4.6 6.3h3.2M6.2 6.3v4.8" />
          <path d="M11.3 7.1a1.1 1.1 0 0 0-1.9.7c0 .9 2 .8 2 1.9a1.2 1.2 0 0 1-2.1.7" />
        </svg>
      );
    case "css":
      return (
        <svg {...common}>
          <path d="M6.2 3 4.8 13M11.2 3 9.8 13M3 6h10.5M2.5 10H13" />
        </svg>
      );
    case "html":
      return (
        <svg {...common}>
          <path d="M5.2 4.5 2 8l3.2 3.5M10.8 4.5 14 8l-3.2 3.5M9 3.5 7 12.5" />
        </svg>
      );
    case "json":
      return (
        <svg {...common}>
          <path d="M5.5 2.5C4.2 2.5 4 3.3 4 4.5v1.8c0 1-.6 1.7-1.5 1.7.9 0 1.5.7 1.5 1.7v1.8c0 1.2.2 2 1.5 2" />
          <path d="M10.5 2.5c1.3 0 1.5.8 1.5 2v1.8c0 1 .6 1.7 1.5 1.7-.9 0-1.5.7-1.5 1.7v1.8c0 1.2-.2 2-1.5 2" />
        </svg>
      );
    case "yaml":
      return (
        <svg {...common}>
          <path d="M3 4h4M3 8h3M3 12h4M9.5 4h3.5M8.5 8H13M9.5 12h3.5" />
        </svg>
      );
    case "markdown":
      return (
        <svg {...common}>
          <rect x="1.8" y="3.2" width="12.4" height="9.6" rx="1.2" />
          <path d="M4.2 10.4V5.8l2 2.5 2-2.5v4.6M11.2 5.8v4.5m-1.5-1.5 1.5 1.6 1.5-1.6" />
        </svg>
      );
    case "image":
      return (
        <svg {...common}>
          <rect x="2" y="2.8" width="12" height="10.4" rx="1.2" />
          <circle cx="5.6" cy="6.2" r="1" fill="currentColor" stroke="none" />
          <path d="M2.5 11.4 6 8l2.5 2.4 2-2 3 3" />
        </svg>
      );
    case "shell":
      return (
        <svg {...common}>
          <rect x="2" y="2.8" width="12" height="10.4" rx="1.2" />
          <path d="M4.6 6.2 6.8 8 4.6 9.8M8.2 9.8h3" />
        </svg>
      );
    case "config":
      return (
        <svg {...common}>
          <circle cx="8" cy="8" r="2.2" />
          <path d="M8 2.2v1.5M8 12.3v1.5M2.2 8h1.5M12.3 8h1.5M3.9 3.9l1.1 1.1M11 11l1.1 1.1M12.1 3.9 11 5M5 11l-1.1 1.1" />
        </svg>
      );
    case "generic":
      return (
        <svg {...common}>
          <path d="M4 2h5.2L12.5 5.3V14H4V2Z" />
          <path d="M9 2v3.5h3.5" />
        </svg>
      );
  }
}
