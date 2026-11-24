/**
 * The shared icon set (pockets-design-update-v2.md §4): inline stroke SVGs,
 * `stroke-width: 2` (2.5 for check and arrow-up), `currentColor`, no icon
 * font and no emoji. Every icon is decoration next to a real label or an
 * `aria-label` on its button, so the SVG itself is `aria-hidden`.
 */

interface IconProps {
  className?: string;
  /** Rendered square size in px (viewBox stays 24). */
  size?: number;
}

function svgProps({ className, size = 16 }: IconProps) {
  return {
    className,
    width: size,
    height: size,
    viewBox: "0 0 24 24",
    fill: "none",
    stroke: "currentColor",
    strokeWidth: 2,
    strokeLinecap: "round" as const,
    strokeLinejoin: "round" as const,
    "aria-hidden": true,
    focusable: false as const,
  };
}

export function IconPlus(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <path d="M12 5v14M5 12h14" />
    </svg>
  );
}

export function IconChevronDown(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <path d="m6 9 6 6 6-6" />
    </svg>
  );
}

export function IconChevronUp(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <path d="m18 15-6-6-6 6" />
    </svg>
  );
}

export function IconCheck(props: IconProps) {
  return (
    <svg {...svgProps(props)} strokeWidth={2.5}>
      <path d="m4 12.5 5 5L20 6.5" />
    </svg>
  );
}

export function IconClose(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <path d="M6 6l12 12M18 6 6 18" />
    </svg>
  );
}

export function IconReload(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <path d="M20 12a8 8 0 1 1-2.34-5.66" />
      <path d="M20 4v4h-4" />
    </svg>
  );
}

export function IconExternalLink(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <path d="M14 4h6v6" />
      <path d="M20 4 11 13" />
      <path d="M18 13v6a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h6" />
    </svg>
  );
}

export function IconFolderOpen(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <path d="M3 18V6a2 2 0 0 1 2-2h4l2 3h7a2 2 0 0 1 2 2v2" />
      <path d="m3 18 3-7h15a1 1 0 0 1 1 1.3l-2.3 6.4a2 2 0 0 1-1.9 1.3H5a2 2 0 0 1-2-2Z" />
    </svg>
  );
}

export function IconCopy(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <rect x="8" y="8" width="12" height="12" rx="2" />
      <path d="M6 16H5a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v1" />
    </svg>
  );
}

export function IconArrowUp(props: IconProps) {
  return (
    <svg {...svgProps(props)} strokeWidth={2.5}>
      <path d="M12 19V5" />
      <path d="m5 12 7-7 7 7" />
    </svg>
  );
}

/** Home: the workspace (chat) section in the nav rail. */
export function IconHome(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <path d="m3 10 9-7 9 7v10a1 1 0 0 1-1 1h-5v-7h-6v7H4a1 1 0 0 1-1-1Z" />
    </svg>
  );
}

/** Filled-left panel: the sidebar-collapse toggle in the header. */
export function IconPanelLeft(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <rect x="3" y="3" width="18" height="18" />
      <path d="M9 3v18" />
    </svg>
  );
}

/** Filled-right panel: the preview-rail collapse toggle in the header. */
export function IconPanelRight(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <rect x="3" y="3" width="18" height="18" />
      <path d="M15 3v18" />
    </svg>
  );
}

/** Four squares: the "all pockets" management page in the nav rail. */
export function IconGrid(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <rect x="3" y="3" width="7" height="7" />
      <rect x="14" y="3" width="7" height="7" />
      <rect x="14" y="14" width="7" height="7" />
      <rect x="3" y="14" width="7" height="7" />
    </svg>
  );
}

/** Drafts rail icon: a page with a folded corner and text lines — the
    same stroke idiom as the rest of the set, no new visual language. */
export function IconDraft(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <path d="M14 3v4a1 1 0 0 0 1 1h4" />
      <path d="M17 21H7a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h7l5 5v11a2 2 0 0 1-2 2Z" />
      <path d="M9 13h6" />
      <path d="M9 17h6" />
    </svg>
  );
}

export function IconArrowRight(props: IconProps) {
  return (
    <svg {...svgProps(props)} strokeWidth={2.5}>
      <path d="M5 12h14" />
      <path d="m12 5 7 7-7 7" />
    </svg>
  );
}

/** Destructive menu rows (v3 §4 "Delete pocket…"), filled like the rest of
    the set's stroke idiom. */
export function IconTrash(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <path d="M3 6h18M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2m3 0v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6" />
    </svg>
  );
}

/** Sun: Light theme indicator. */
export function IconSun(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <circle cx="12" cy="12" r="4" />
      <path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M6.34 17.66l-1.41 1.41M19.07 4.93l-1.41 1.41" />
    </svg>
  );
}

/** Moon: Dark theme indicator. */
export function IconMoon(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79Z" />
    </svg>
  );
}

/** Monitor: System theme indicator. */
export function IconMonitor(props: IconProps) {
  return (
    <svg {...svgProps(props)}>
      <rect x="2" y="3" width="20" height="14" rx="1" />
      <path d="M8 21h8M12 17v4" />
    </svg>
  );
}
