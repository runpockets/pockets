import { afterEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import ExternalWorkspace, {
  PLACEHOLDER_CONTAINERFILE_MARKER,
} from "./ExternalWorkspace";
import type { PreviewApiAccess } from "./useProjectPreview";

vi.mock("./DevelopmentPanel", () => ({
  default: () => <div data-testid="dev-panel" />,
}));
vi.mock("./RuntimeSettings", () => ({
  default: () => <div data-testid="runtime-settings" />,
}));
vi.mock("./PreviewNetworkSettings", () => ({
  default: () => <div data-testid="preview-network" />,
}));
vi.mock("./HubSubmissionPanel", () => ({
  default: () => <div data-testid="hub-submission" />,
}));

const idleApiAccess: PreviewApiAccess = {
  review: null,
  busy: false,
  canEditHosts: true,
  error: null,
  decide: vi.fn(),
};

function renderWorkspace(faviconCacheKey?: string) {
  return render(
    <ExternalWorkspace
      projectId="proj-123"
      projectName="CoinKiwi"
      faviconCacheKey={faviconCacheKey}
      linked={false}
      previewApiAccess={idleApiAccess}
      building={false}
      sidebarCollapsed={false}
      previewCollapsed={false}
      onToggleSidebar={vi.fn()}
      onTogglePreview={vi.fn()}
      onSourceChanged={vi.fn()}
      controlsRef={vi.fn()}
    />,
  );
}

describe("ExternalWorkspace manage header favicon", () => {
  afterEach(cleanup);

  it("shows the favicon to the left of the Pocket title once loaded", () => {
    const { container } = renderWorkspace("abcdef123456-1");
    const title = screen.getByText("CoinKiwi");
    const img = container.querySelector<HTMLImageElement>(
      "img.external-workspace-favicon",
    );
    expect(img).not.toBeNull();
    expect(img?.getAttribute("src")).toBe(
      "/api/projects/proj-123/favicon?v=abcdef123456-1",
    );
    expect(img?.dataset.loaded).toBeUndefined();
    expect(
      img!.compareDocumentPosition(title) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).not.toBe(0);

    fireEvent.load(img!);
    expect(img?.dataset.loaded).toBe("true");
  });

  it("removes the favicon entirely without a placeholder when the Pocket has none", () => {
    const { container } = renderWorkspace("abcdef123456-0");
    const img = container.querySelector<HTMLImageElement>(
      "img.external-workspace-favicon",
    );
    expect(img).not.toBeNull();

    fireEvent.error(img!);
    expect(
      container.querySelector("img.external-workspace-favicon"),
    ).toBeNull();
    expect(
      container.querySelector(".external-workspace-title")?.textContent,
    ).toBe("CoinKiwi");
  });

  it("retries loading when faviconCacheKey changes (e.g. after starting the Pocket)", () => {
    const { container, rerender } = renderWorkspace("abcdef123456-0");
    const initial = container.querySelector<HTMLImageElement>(
      "img.external-workspace-favicon",
    );
    fireEvent.error(initial!);
    expect(
      container.querySelector("img.external-workspace-favicon"),
    ).toBeNull();

    rerender(
      <ExternalWorkspace
        projectId="proj-123"
        projectName="CoinKiwi"
        faviconCacheKey="abcdef123456-1"
        linked={false}
        previewApiAccess={idleApiAccess}
        building={false}
        sidebarCollapsed={false}
        previewCollapsed={false}
        onToggleSidebar={vi.fn()}
        onTogglePreview={vi.fn()}
        onSourceChanged={vi.fn()}
        controlsRef={vi.fn()}
      />,
    );

    const retried = container.querySelector<HTMLImageElement>(
      "img.external-workspace-favicon",
    );
    expect(retried).not.toBeNull();
    expect(retried?.getAttribute("src")).toBe(
      "/api/projects/proj-123/favicon?v=abcdef123456-1",
    );
    fireEvent.load(retried!);
    expect(retried?.dataset.loaded).toBe("true");
  });

  it("stays visible when the browser serves a cached favicon synchronously", () => {
    const completeDescriptor = Object.getOwnPropertyDescriptor(
      HTMLImageElement.prototype,
      "complete",
    );
    const widthDescriptor = Object.getOwnPropertyDescriptor(
      HTMLImageElement.prototype,
      "naturalWidth",
    );
    Object.defineProperty(HTMLImageElement.prototype, "complete", {
      configurable: true,
      get: () => true,
    });
    Object.defineProperty(HTMLImageElement.prototype, "naturalWidth", {
      configurable: true,
      get: () => 32,
    });
    try {
      const { container } = renderWorkspace("abcdef123456-1");
      const img = container.querySelector<HTMLImageElement>(
        "img.external-workspace-favicon",
      );
      expect(img).not.toBeNull();
      expect(img?.dataset.loaded).toBe("true");
    } finally {
      if (completeDescriptor) {
        Object.defineProperty(
          HTMLImageElement.prototype,
          "complete",
          completeDescriptor,
        );
      }
      if (widthDescriptor) {
        Object.defineProperty(
          HTMLImageElement.prototype,
          "naturalWidth",
          widthDescriptor,
        );
      }
    }
  });
});

describe("ExternalWorkspace pocketize handoff card", () => {
  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
  });

  it("shows a copyable AI handoff prompt when a linked project has a placeholder Containerfile", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });

    render(
      <ExternalWorkspace
        projectId="proj-123"
        projectName="Budget Tracker"
        linked
        containerfile={`${PLACEHOLDER_CONTAINERFILE_MARKER}\nFROM node:24-alpine\n`}
        projectFiles={["requirements.txt", "Dockerfile", "pocket.yaml"]}
        previewApiAccess={idleApiAccess}
        building={false}
        sidebarCollapsed={false}
        previewCollapsed={false}
        onToggleSidebar={vi.fn()}
        onTogglePreview={vi.fn()}
        onSourceChanged={vi.fn()}
        controlsRef={vi.fn()}
      />,
    );

    const card = screen.getByRole("region", { name: "Finish pocketizing" });
    expect(card.textContent).toContain("Budget Tracker (Python)");
    expect(card.textContent).toContain("adapting the existing Dockerfile");

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Copy AI prompt" }));
    });
    expect(writeText).toHaveBeenCalledWith(
      expect.stringContaining("Read POCKETS.md"),
    );
    expect(screen.getByText("Copied prompt")).toBeTruthy();
  });

  it("shows a dismissible clean-room recreation prompt when pocketized from an existing folder", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    const onDismiss = vi.fn();
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });

    render(
      <ExternalWorkspace
        projectId="proj-123"
        projectName="Dabn"
        linked
        pocketizeSource={{
          folderPath: "~/Code/personal/dabn",
          folderName: "dabn",
          detectedStack: "Rust",
        }}
        onDismissPocketizeSource={onDismiss}
        previewApiAccess={idleApiAccess}
        building={false}
        sidebarCollapsed={false}
        previewCollapsed={false}
        onToggleSidebar={vi.fn()}
        onTogglePreview={vi.fn()}
        onSourceChanged={vi.fn()}
        controlsRef={vi.fn()}
      />,
    );

    const card = screen.getByRole("region", { name: "Finish pocketizing" });
    expect(card.textContent).toContain("~/Code/personal/dabn (Rust)");
    expect(card.textContent).toContain("without touching the original project");

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Copy AI prompt" }));
    });
    expect(writeText).toHaveBeenCalledWith(
      expect.stringContaining(
        "recreate the Dabn app from ~/Code/personal/dabn",
      ),
    );

    fireEvent.click(
      screen.getByRole("button", { name: "Dismiss pocketize prompt" }),
    );
    expect(onDismiss).toHaveBeenCalledTimes(1);
  });

  it("tailors the recreation prompt when the pocket folder lives inside the selected project", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });

    render(
      <ExternalWorkspace
        projectId="proj-123"
        projectName="Dabn"
        linked
        pocketizeSource={{
          folderPath: "~/Code/personal/dabn",
          folderName: "dabn",
          detectedStack: "Rust",
          targetSubfolder: "pocket",
        }}
        previewApiAccess={idleApiAccess}
        building={false}
        sidebarCollapsed={false}
        previewCollapsed={false}
        onToggleSidebar={vi.fn()}
        onTogglePreview={vi.fn()}
        onSourceChanged={vi.fn()}
        controlsRef={vi.fn()}
      />,
    );

    const card = screen.getByRole("region", { name: "Finish pocketizing" });
    expect(card.textContent).toContain("pocket/AGENTS.md");
    expect(card.textContent).toContain("without touching your existing files");

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Copy AI prompt" }));
    });
    expect(writeText).toHaveBeenCalledWith(
      expect.stringContaining("recreate Dabn (Rust) inside pocket/"),
    );
  });

  it("hides the handoff card once Containerfile has been adapted", () => {
    render(
      <ExternalWorkspace
        projectId="proj-123"
        projectName="Budget Tracker"
        linked
        containerfile="FROM python:3.12-slim\nUSER app\n"
        projectFiles={["requirements.txt", "pocket.yaml", "Containerfile"]}
        previewApiAccess={idleApiAccess}
        building={false}
        sidebarCollapsed={false}
        previewCollapsed={false}
        onToggleSidebar={vi.fn()}
        onTogglePreview={vi.fn()}
        onSourceChanged={vi.fn()}
        controlsRef={vi.fn()}
      />,
    );

    expect(
      screen.queryByRole("region", { name: "Finish pocketizing" }),
    ).toBeNull();
  });
});
