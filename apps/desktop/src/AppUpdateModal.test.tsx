import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import AppUpdateModal, { type AppUpdateStatus } from "./AppUpdateModal";

const mockInvoke = vi.fn();

vi.mock("./api", () => ({
  invoke: (...args: unknown[]) => mockInvoke(...args),
}));

afterEach(() => {
  cleanup();
  mockInvoke.mockReset();
});

const UPDATE_AVAILABLE: AppUpdateStatus = {
  currentVersion: "0.1.0",
  latestVersion: "0.1.1",
  updateAvailable: true,
  releaseUrl: "https://github.com/runpockets/pockets/releases/tag/v0.1.1",
  notes: ["Add Pockets brand logo", "Prefer active Podman binary"],
  publishedAt: "2026-11-25T00:00:00Z",
  installMethod: "homebrew",
  canAutoUpdate: true,
  manualCommand:
    "brew update && brew upgrade --ignore-dependencies pockets && brew services restart pockets",
  error: null,
};

describe("AppUpdateModal", () => {
  it("renders release highlights and triggers apply_app_update on click", async () => {
    mockInvoke.mockResolvedValueOnce({
      updatedTo: "0.1.1",
      restarting: true,
      message: "Updated Pockets to v0.1.1. Restarting the daemon…",
    });

    const onClose = vi.fn();
    render(
      <AppUpdateModal
        open
        status={UPDATE_AVAILABLE}
        checking={false}
        onClose={onClose}
        onRecheck={vi.fn()}
      />,
    );

    expect(screen.getByText("Pockets update available")).toBeTruthy();
    expect(screen.getByText("Version v0.1.1 is ready to install.")).toBeTruthy();
    expect(screen.getByText("Add Pockets brand logo")).toBeTruthy();
    expect(screen.getByText("Prefer active Podman binary")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Update now" }));

    await waitFor(() => {
      expect(mockInvoke).toHaveBeenCalledWith("apply_app_update");
      expect(screen.getByText("Restarting Pockets…")).toBeTruthy();
    });
  });

  it("shows manual command for development builds without auto-update button", () => {
    render(
      <AppUpdateModal
        open
        status={{
          ...UPDATE_AVAILABLE,
          installMethod: "development",
          canAutoUpdate: false,
          manualCommand:
            "git pull && npm run build:web && cargo build -p pockets-cli",
        }}
        checking={false}
        onClose={vi.fn()}
        onRecheck={vi.fn()}
      />,
    );

    expect(screen.queryByRole("button", { name: "Update now" })).toBeNull();
    expect(
      screen.getByText(
        "git pull && npm run build:web && cargo build -p pockets-cli",
      ),
    ).toBeTruthy();
  });

  it("shows up-to-date state and supports Check again", () => {
    const onRecheck = vi.fn();
    render(
      <AppUpdateModal
        open
        status={{
          ...UPDATE_AVAILABLE,
          latestVersion: "0.1.0",
          updateAvailable: false,
          notes: [],
        }}
        checking={false}
        onClose={vi.fn()}
        onRecheck={onRecheck}
      />,
    );

    expect(screen.getByText("Pockets is up to date")).toBeTruthy();
    expect(screen.getByText("You're running v0.1.0.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Check again" }));
    expect(onRecheck).toHaveBeenCalledTimes(1);
  });
});
