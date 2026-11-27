import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import ExternalWorkspace from "./ExternalWorkspace";
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
