import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { invoke } from "./api";
import CreationPanel, {
  type PocketStatus,
  type Project,
} from "./CreationPanel";
import { TailscaleStatusPanel } from "./TailscaleShare";

vi.mock("./api", () => ({ invoke: vi.fn(), listen: vi.fn() }));
vi.mock("./RevisionHistory", () => ({
  default: () => null,
}));

const invokeMock = vi.mocked(invoke);

const INSTALLED_PROJECT: Project = {
  id: "a".repeat(32),
  manifest: {
    metadata: {
      id: "notes",
      name: "Notes Pocket",
      version: "1.0.0",
      description: "Personal notes.",
    },
    runtime: { image: "node:24-alpine", architectures: ["arm64"] },
    web: { port: 3000, healthcheck: "/health" },
    storage: { name: "notes-data", path: "/data" },
  },
  phase: "installed",
  builtImageId: `sha256:${"b".repeat(64)}`,
  files: ["Containerfile", "server.js"],
  totalBytes: 1024,
  containerfile: "FROM node:24-alpine\n",
  sourceDigest: `sha256:${"c".repeat(64)}`,
};

const RUNNING_STATUS: PocketStatus = {
  state: "running",
  url: "http://notes.localhost:18788",
  message: "Running",
  ready: true,
  tailscale: {
    enabled: false,
    active: false,
    url: null,
    message: null,
  },
};

const TAILSCALE_READY = {
  state: "ready" as const,
  message: "Tailscale 1.104.6 is ready on macbook-pro.tail00000.ts.net.",
  dnsName: "macbook-pro.tail00000.ts.net",
  version: "1.104.6",
};

beforeEach(() => {
  invokeMock.mockReset();
});

// Tailscale lives on the Manage view's card, which CreationPanel portals into
// a host element supplied by App. Tests stand in for that host.
const hosts: HTMLElement[] = [];
function manageHost(): HTMLElement {
  const host = document.createElement("div");
  host.setAttribute("data-testid", "manage-host");
  document.body.appendChild(host);
  hosts.push(host);
  return host;
}

afterEach(() => {
  cleanup();
  hosts.splice(0).forEach((host) => host.remove());
});

describe("Tailscale sharing in CreationPanel", () => {
  it("enables sharing on a running Pocket, renders named URL, direct fallback, and QR code, opens enlarged QR modal, and disables sharing", async () => {
    const namedUrl = "https://macbook-pro.tail00000.ts.net/notes";
    const directUrl = "https://macbook-pro.tail00000.ts.net:18788";
    invokeMock.mockImplementation(async (command, args) => {
      if (command === "list_projects") return [INSTALLED_PROJECT];
      if (command === "pocket_thumb") return null;
      if (command === "pocket_thumb_capture") return null;
      if (command === "project_status") return RUNNING_STATUS;
      if (command === "detect_tailscale") return TAILSCALE_READY;
      if (command === "set_project_tailscale") {
        const { enabled } = args as { id: string; enabled: boolean };
        return {
          ...RUNNING_STATUS,
          tailscale: enabled
            ? {
                enabled: true,
                active: true,
                url: namedUrl,
                directUrl,
                portalUrl: "https://macbook-pro.tail00000.ts.net",
                message: null,
              }
            : { enabled: false, active: false, url: null, message: null },
        };
      }
      throw new Error(`Unexpected command: ${command}`);
    });

    const host = manageHost();
    render(
      <CreationPanel
        runtimeReady
        page="live"
        workspace={{ project: INSTALLED_PROJECT, host }}
      />,
    );
    const manage = within(host);

    const summary = await manage.findByText("Tailscale");
    const details = summary.closest("details")!;
    details.open = true;
    fireEvent(details, new Event("toggle"));

    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("detect_tailscale"),
    );

    const toggle = manage.getByRole("switch", {
      name: "Share Notes Pocket on Tailscale",
    });
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    fireEvent.click(toggle);

    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_project_tailscale", {
        id: INSTALLED_PROJECT.id,
        enabled: true,
      }),
    );

    expect(
      await manage.findByText("macbook-pro.tail00000.ts.net/notes"),
    ).toBeTruthy();
    expect(manage.getByText("macbook-pro.tail00000.ts.net:18788")).toBeTruthy();
    expect(
      manage.getByRole("img", {
        name: "QR code for Notes Pocket Tailscale URL",
      }),
    ).toBeTruthy();

    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      configurable: true,
    });
    // Top copy icon copies the named shortcut; Copy URL next to QR copies direct URL.
    fireEvent.click(manage.getByRole("button", { name: "Copy Tailscale URL" }));
    expect(writeText).toHaveBeenLastCalledWith(namedUrl);
    fireEvent.click(manage.getByRole("button", { name: "Copy URL" }));
    expect(writeText).toHaveBeenLastCalledWith(directUrl);

    // Quick Tailscale pill next to the local address opens the QR modal with the direct URL.
    fireEvent.click(
      manage.getByRole("button", {
        name: "Show Tailscale QR code for Notes Pocket",
      }),
    );
    const dialog = await screen.findByRole("dialog", {
      name: "Notes Pocket on Tailscale",
    });
    expect(dialog.textContent).toContain(directUrl);

    // Escape closes the QR modal.
    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() =>
      expect(
        screen.queryByRole("dialog", { name: "Notes Pocket on Tailscale" }),
      ).toBeNull(),
    );

    // Turning sharing back off clears the URL and QR code.
    fireEvent.click(toggle);
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_project_tailscale", {
        id: INSTALLED_PROJECT.id,
        enabled: false,
      }),
    );
    await waitFor(() =>
      expect(
        manage.queryByText("macbook-pro.tail00000.ts.net/notes"),
      ).toBeNull(),
    );
  });

  it("shows Paused while stopped and Reconnect when a running Pocket route is inactive", async () => {
    invokeMock.mockImplementation(async (command) => {
      if (command === "list_projects") return [INSTALLED_PROJECT];
      if (command === "pocket_thumb") return null;
      if (command === "pocket_thumb_capture") return null;
      if (command === "detect_tailscale") return TAILSCALE_READY;
      if (command === "project_status") {
        return {
          ...RUNNING_STATUS,
          tailscale: {
            enabled: true,
            active: false,
            url: null,
            message:
              "Tailscale route is not active. Click Reconnect to resume sharing on your tailnet.",
          },
        };
      }
      if (command === "set_project_tailscale") {
        return {
          ...RUNNING_STATUS,
          tailscale: {
            enabled: true,
            active: true,
            url: "https://macbook-pro.tail00000.ts.net:18788",
            message: null,
          },
        };
      }
      throw new Error(`Unexpected command: ${command}`);
    });

    const host = manageHost();
    render(
      <CreationPanel
        runtimeReady
        page="live"
        workspace={{ project: INSTALLED_PROJECT, host }}
      />,
    );
    const manage = within(host);
    expect(await manage.findByText("Needs reconnect")).toBeTruthy();

    fireEvent.click(manage.getByRole("button", { name: "Reconnect" }));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("set_project_tailscale", {
        id: INSTALLED_PROJECT.id,
        enabled: true,
      }),
    );
    expect(await manage.findByText("Shared")).toBeTruthy();
  });

  it("shows actionable Tailscale setup guidance when HTTPS certificates are disabled", async () => {
    invokeMock.mockImplementation(async (command) => {
      if (command === "list_projects") return [INSTALLED_PROJECT];
      if (command === "pocket_thumb") return null;
      if (command === "pocket_thumb_capture") return null;
      if (command === "project_status") return RUNNING_STATUS;
      if (command === "detect_tailscale") {
        return {
          state: "httpsDisabled",
          message:
            "Tailscale HTTPS certificates are disabled for your tailnet.",
          dnsName: "macbook-pro.tail00000.ts.net",
          version: "1.104.6",
        };
      }
      throw new Error(`Unexpected command: ${command}`);
    });

    const host = manageHost();
    render(
      <CreationPanel
        runtimeReady
        page="live"
        workspace={{ project: INSTALLED_PROJECT, host }}
      />,
    );
    const manage = within(host);

    const summary = await manage.findByText("Tailscale");
    const details = summary.closest("details")!;
    details.open = true;
    fireEvent(details, new Event("toggle"));

    expect(
      await manage.findByText(
        "Tailscale HTTPS certificates are disabled for your tailnet.",
      ),
    ).toBeTruthy();
    const toggle = manage.getByRole("switch", {
      name: "Share Notes Pocket on Tailscale",
    });
    expect(toggle.hasAttribute("disabled")).toBe(true);
  });
});

describe("TailscaleStatusPanel", () => {
  it("renders Ready status, MagicDNS hostname, Pocket launcher QR, and Refresh action", () => {
    const onRefresh = vi.fn();
    render(
      <TailscaleStatusPanel
        report={{
          status: "ready",
          value: {
            ...TAILSCALE_READY,
            portalUrl: "https://macbook-pro.tail00000.ts.net",
          },
        }}
        refreshing={false}
        onRefresh={onRefresh}
      />,
    );
    expect(screen.getByText("Ready")).toBeTruthy();
    expect(screen.getByText("macbook-pro.tail00000.ts.net")).toBeTruthy();
    expect(screen.getByText("Pocket launcher")).toBeTruthy();
    expect(
      screen.getByText("https://macbook-pro.tail00000.ts.net"),
    ).toBeTruthy();
    expect(
      screen.getByRole("img", {
        name: "QR code for Pockets Tailscale launcher",
      }),
    ).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(onRefresh).toHaveBeenCalledTimes(1);
  });
});
