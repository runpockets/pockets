/**
 * Browser-native RPC, backup transfer, and folder-picker bridge for the
 * cross-platform Pockets daemon (`pockets serve`).
 *
 * Replaces `@tauri-apps/api/core` and `@tauri-apps/api/event` so the React UI
 * runs identically in any desktop or mobile browser on macOS, Linux, and Windows.
 */

export interface DirectoryEntry {
  name: string;
  path: string;
  hasPocketYaml: boolean;
}

export interface DirectoryListing {
  currentPath: string;
  parentPath: string | null;
  hasPocketYaml: boolean;
  entries: DirectoryEntry[];
  truncated: boolean;
}

export interface FolderPickRequest {
  title: string;
  description?: string;
  confirmLabel: string;
  /** When true, highlights folders containing `pocket.yaml`. */
  preferPocketFolder?: boolean;
  /** Optional subfolder name that will be created inside the chosen folder. */
  createSubfolder?: string;
}

type FolderPickerHandler = (
  request: FolderPickRequest,
) => Promise<string | null>;

let folderPickerHandler: FolderPickerHandler | null = null;
let authRequiredHandler: (() => void) | null = null;

/** Registers the UI modal responsible for folder selection in the browser. */
export function registerFolderPicker(
  handler: FolderPickerHandler | null,
): void {
  folderPickerHandler = handler;
}

/** Registers a callback invoked when the local daemon returns HTTP 401. */
export function registerAuthRequired(handler: (() => void) | null): void {
  authRequiredHandler = handler;
}

/** Authenticates this browser using a session token or full bootstrap URL. */
export async function authenticateSession(tokenOrUrl: string): Promise<void> {
  const response = await fetch("/auth/session", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ token: tokenOrUrl }),
  });
  let payload: { ok?: boolean; error?: string } | null = null;
  try {
    payload = (await response.json()) as { ok?: boolean; error?: string };
  } catch {
    // fall through
  }
  if (!response.ok || payload?.ok === false) {
    throw new Error(
      payload?.error ?? `Could not connect browser (HTTP ${response.status}).`,
    );
  }
}

async function pickFolder(request: FolderPickRequest): Promise<string | null> {
  if (folderPickerHandler !== null) {
    return folderPickerHandler(request);
  }
  const entered = window.prompt(`${request.title}\n${request.description}`);
  if (entered === null || entered.trim() === "") return null;
  return entered.trim();
}

async function rpcCall<T>(
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  const response = await fetch(`/api/rpc/${encodeURIComponent(command)}`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(args ?? {}),
  });

  let payload: { ok?: boolean; result?: T; error?: string } | null = null;
  try {
    payload = (await response.json()) as {
      ok?: boolean;
      result?: T;
      error?: string;
    };
  } catch {
    // Non-JSON proxy or server error falls through to status message below.
  }

  if (response.status === 401) {
    authRequiredHandler?.();
  }

  if (!response.ok || payload?.ok === false) {
    throw new Error(
      payload?.error ??
        `Pockets daemon returned HTTP ${response.status} for ${command}.`,
    );
  }
  return payload?.result as T;
}

function parseFilename(contentDisposition: string | null): string {
  if (contentDisposition !== null) {
    const match = /filename="([^"]+)"/i.exec(contentDisposition);
    if (match?.[1]) return match[1];
  }
  return "pocket-backup.tar";
}

async function downloadBackup(id: string): Promise<string | null> {
  const response = await fetch(
    `/api/projects/${encodeURIComponent(id)}/backup`,
    {
      method: "GET",
      credentials: "include",
    },
  );
  if (!response.ok) {
    let message = `Could not export backup (HTTP ${response.status}).`;
    try {
      const payload = (await response.json()) as { error?: string };
      if (payload.error) message = payload.error;
    } catch {
      // keep default message
    }
    throw new Error(message);
  }
  const fileName = parseFilename(response.headers.get("Content-Disposition"));
  const blob = await response.blob();
  const objectUrl = URL.createObjectURL(blob);
  try {
    const anchor = document.createElement("a");
    anchor.href = objectUrl;
    anchor.download = fileName;
    document.body.appendChild(anchor);
    anchor.click();
    anchor.remove();
  } finally {
    window.setTimeout(() => URL.revokeObjectURL(objectUrl), 1000);
  }
  return fileName;
}

function selectBackupFile(): Promise<File | null> {
  return new Promise((resolve) => {
    const input = document.createElement("input");
    input.type = "file";
    input.accept = ".tar,application/x-tar";
    input.style.display = "none";
    let settled = false;
    const finish = (file: File | null) => {
      if (settled) return;
      settled = true;
      window.removeEventListener("focus", onFocus);
      input.remove();
      resolve(file);
    };
    const onFocus = () => {
      // Browsers fire focus back to the window before change when cancelled.
      window.setTimeout(() => {
        if (!settled) finish(input.files?.[0] ?? null);
      }, 300);
    };
    input.addEventListener("change", () => {
      finish(input.files?.[0] ?? null);
    });
    input.addEventListener("cancel", () => finish(null));
    window.addEventListener("focus", onFocus);
    document.body.appendChild(input);
    input.click();
  });
}

async function uploadBackup<T>(id: string): Promise<T | null> {
  const file = await selectBackupFile();
  if (file === null) return null;
  const response = await fetch(
    `/api/projects/${encodeURIComponent(id)}/backup`,
    {
      method: "POST",
      credentials: "include",
      headers: {
        "Content-Type": "application/x-tar",
        "X-Backup-Filename": file.name,
      },
      body: file,
    },
  );
  let payload: { ok?: boolean; result?: T; error?: string } | null = null;
  try {
    payload = (await response.json()) as {
      ok?: boolean;
      result?: T;
      error?: string;
    };
  } catch {
    // fall through
  }
  if (response.status === 401) {
    authRequiredHandler?.();
  }

  if (!response.ok || payload?.ok === false) {
    throw new Error(
      payload?.error ?? `Could not restore backup (HTTP ${response.status}).`,
    );
  }
  return payload?.result as T;
}

/**
 * Drop-in replacement for Tauri's `invoke`, transparently handling browser
 * folder selection, browser backup download/upload, clipboard copy, and tab
 * opening for local Pockets.
 */
export async function invoke<T = unknown>(
  command: string,
  args?: Record<string, unknown>,
): Promise<T> {
  if (command === "create_development_project" && !args?.parentPath) {
    const folderName =
      typeof args?.folderName === "string" && args.folderName.trim() !== ""
        ? args.folderName.trim()
        : undefined;
    const parentPath = await pickFolder({
      title: "Choose parent folder",
      confirmLabel: "Create here",
      createSubfolder: folderName,
    });
    if (parentPath === null) return null as T;
    return rpcCall<T>(command, { ...args, parentPath });
  }

  if (command === "export_development_project" && !args?.parentPath) {
    const parentPath = await pickFolder({
      title: "Export saved source",
      description:
        "Choose a parent folder where Pockets will create an editable copy.",
      confirmLabel: "Export here",
    });
    if (parentPath === null) return null as T;
    return rpcCall<T>(command, { ...args, parentPath });
  }

  if (command === "link_development_folder" && !args?.folderPath) {
    const folderPath = await pickFolder({
      title: "Link Pocket folder",
      description:
        "Select a folder containing a pocket.yaml manifest to link for live preview.",
      confirmLabel: "Link folder",
      preferPocketFolder: true,
    });
    if (folderPath === null) return null as T;
    return rpcCall<T>(command, { ...args, folderPath });
  }

  if (command === "import_project" && !args?.folderPath) {
    const folderPath = await pickFolder({
      title: "Import Pocket folder",
      description: "Select a folder containing pocket.yaml to import.",
      confirmLabel: "Import folder",
      preferPocketFolder: true,
    });
    if (folderPath === null) return null as T;
    return rpcCall<T>(command, { ...args, folderPath });
  }

  if (command === "export_project_data") {
    return downloadBackup(String(args?.id ?? "")) as Promise<T>;
  }

  if (command === "restore_project_data") {
    return uploadBackup<T>(String(args?.id ?? "")) as Promise<T>;
  }

  if (
    command === "open_project" ||
    command === "open_pocket" ||
    command === "project_preview_open"
  ) {
    const url = await rpcCall<string>(command, args);
    if (typeof url === "string" && /^https?:\/\//i.test(url)) {
      window.open(url, "_blank", "noopener,noreferrer");
    }
    return undefined as T;
  }

  if (command === "copy_development_location") {
    const path = await rpcCall<string>(command, args);
    if (typeof path === "string" && path.length > 0) {
      try {
        await navigator.clipboard?.writeText(path);
      } catch {
        // Host clipboard already attempted natively; ignore browser permission denials.
      }
    }
    return undefined as T;
  }

  return rpcCall<T>(command, args);
}

/**
 * Compatibility event listener stub (used only by legacy thumbnail updates).
 */
export async function listen<T>(
  _event: string,
  _handler: (event: { payload: T }) => void,
): Promise<() => void> {
  return () => {};
}
