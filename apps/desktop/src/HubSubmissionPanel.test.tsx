import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { invoke } from "./api";
import HubSubmissionPanel, {
  type HubSubmissionReport,
} from "./HubSubmissionPanel";

vi.mock("./api", () => ({ invoke: vi.fn() }));
const native = vi.mocked(invoke);

const pendingReport: HubSubmissionReport = {
  projectId: "p1",
  pocketId: "habit-tracker",
  pocketName: "Habit Tracker",
  detectedRepo: "alice/habit-tracker",
  resolvedRepo: "alice/habit-tracker",
  alreadyInHub: false,
  readyToSubmit: true,
  remoteVerified: false,
  checks: [
    {
      id: "manifest",
      label: "pocket.yaml manifest",
      status: "pass",
      detail: "Valid manifest (id 'habit-tracker', v0.1.0, arm64 + amd64).",
    },
    {
      id: "containerfile",
      label: "Containerfile security",
      status: "pass",
      detail: "Non-root USER set; no unsafe build patterns or hardcoded secrets.",
    },
    {
      id: "hygiene",
      label: "Repository hygiene",
      status: "pass",
      detail: "README.md and LICENSE present; no committed secrets or symlinks.",
    },
    {
      id: "github",
      label: "Public GitHub repository",
      status: "pending",
      detail: "Using alice/habit-tracker. Click Verify GitHub repo.",
    },
  ],
  catalogEntryJson: '{\n  "repo": "alice/habit-tracker",\n  "verified": false\n}',
  prEditUrl: "https://github.com/runpockets/pockets-hub/edit/main/catalog.json",
  issueSubmitUrl:
    "https://github.com/runpockets/pockets-hub/issues/new?template=submit-pocket.yml",
};

describe("HubSubmissionPanel", () => {
  beforeEach(() => {
    native.mockReset();
  });
  afterEach(cleanup);

  it("runs readiness checks on expand, requires remote verification before enabling PR, and opens PR URL", async () => {
    const openSpy = vi.spyOn(window, "open").mockReturnValue(null);
    native.mockResolvedValueOnce(pendingReport).mockResolvedValueOnce({
      ...pendingReport,
      remoteVerified: true,
      checks: pendingReport.checks.map((c) =>
        c.id === "github"
          ? {
              ...c,
              status: "pass",
              detail: "Verified public repo alice/habit-tracker.",
            }
          : c,
      ),
    });

    render(<HubSubmissionPanel projectId="p1" />);
    const summary = screen.getByText("Submit to Pockets Hub");
    fireEvent.click(summary);

    expect(
      await screen.findByText("Containerfile security"),
    ).toBeTruthy();
    expect(
      (screen.getByPlaceholderText("owner/repo") as HTMLInputElement).value,
    ).toBe("alice/habit-tracker");

    const prBtn = screen.getByRole("button", { name: /Open PR on GitHub/i });
    expect(prBtn.hasAttribute("disabled")).toBe(true);

    await act(async () => {
      fireEvent.click(
        screen.getByRole("button", { name: "Verify GitHub repo" }),
      );
    });
    expect(native).toHaveBeenLastCalledWith("check_hub_submission", {
      id: "p1",
      repo: "alice/habit-tracker",
      verifyRemote: true,
    });

    await waitFor(() =>
      expect(
        screen
          .getByRole("button", { name: /Open PR on GitHub/i })
          .hasAttribute("disabled"),
      ).toBe(false),
    );
    fireEvent.click(screen.getByRole("button", { name: /Open PR on GitHub/i }));
    expect(openSpy).toHaveBeenCalledWith(
      "https://github.com/runpockets/pockets-hub/edit/main/catalog.json",
      "_blank",
      "noopener,noreferrer",
    );
    openSpy.mockRestore();
  });

  it("shows Listed in Pockets Hub when the Pocket is already in the catalog", async () => {
    native.mockResolvedValueOnce({
      ...pendingReport,
      alreadyInHub: true,
      readyToSubmit: false,
    });

    render(<HubSubmissionPanel projectId="p1" />);
    fireEvent.click(screen.getByText("Submit to Pockets Hub"));

    expect(await screen.findByText("Listed in Pockets Hub")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /Open PR/i })).toBeNull();
  });
});
