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
import UpstreamUpdates, {
  buildAgentMergePrompt,
  formatUpstreamDelta,
  type UpstreamPullResult,
  type UpstreamStatus,
} from "./UpstreamUpdates";

vi.mock("./api", () => ({ invoke: vi.fn() }));

const invokeMock = vi.mocked(invoke);

const UPDATE_AVAILABLE: UpstreamStatus = {
  state: "updateAvailable",
  repo: "runpockets/coinkiwi",
  repoUrl: "https://github.com/runpockets/coinkiwi",
  branch: "main",
  upstreamRef: "origin/main",
  localCommit: "abc1234",
  upstreamCommit: "def5678",
  ahead: 0,
  behind: 2,
  localDirty: false,
  currentVersion: "0.4.0",
  upstreamVersion: "0.5.0",
  notes: ["Add price alerts", "Fix CSV export"],
  fetchError: null,
};

beforeEach(() => {
  invokeMock.mockReset();
});

afterEach(() => {
  cleanup();
});

describe("UpstreamUpdates helpers", () => {
  it("formats version transitions when bumped and commit counts otherwise", () => {
    expect(formatUpstreamDelta(UPDATE_AVAILABLE)).toBe("v0.4.0 → v0.5.0");
    expect(
      formatUpstreamDelta({
        ...UPDATE_AVAILABLE,
        upstreamVersion: "0.4.0",
        behind: 1,
      }),
    ).toBe("1 new commit");
    expect(
      formatUpstreamDelta({
        ...UPDATE_AVAILABLE,
        upstreamVersion: "v0.4.0",
        behind: 3,
      }),
    ).toBe("3 new commits");
  });

  it("builds a clear agent merge prompt referencing the upstream ref", () => {
    expect(buildAgentMergePrompt(UPDATE_AVAILABLE)).toContain("origin/main");
  });
});

describe("UpstreamUpdates", () => {
  it("renders nothing for projects with no git remote", async () => {
    invokeMock.mockResolvedValue({
      state: "noRemote",
      ahead: 0,
      behind: 0,
      localDirty: false,
    } satisfies UpstreamStatus);

    const { container } = render(
      <UpstreamUpdates projectId="p-1" built workingDigest="d-1" />,
    );
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("check_project_upstream", {
        id: "p-1",
        force: false,
      }),
    );
    expect(container.firstChild).toBeNull();
  });

  it("auto-opens when an update is available and fast-forwards on Pull update", async () => {
    const onPulled = vi.fn();
    let pulled = false;
    invokeMock.mockImplementation(async (command) => {
      if (command === "check_project_upstream") {
        return pulled
          ? ({
              ...UPDATE_AVAILABLE,
              state: "upToDate",
              localCommit: "def5678",
              behind: 0,
              currentVersion: "0.5.0",
              notes: [],
            } satisfies UpstreamStatus)
          : UPDATE_AVAILABLE;
      }
      if (command === "pull_project_upstream") {
        pulled = true;
        return {
          previousCommit: "abc1234",
          currentCommit: "def5678",
          updated: true,
        } satisfies UpstreamPullResult;
      }
      throw new Error(`Unexpected command: ${command}`);
    });

    render(
      <UpstreamUpdates
        projectId="p-1"
        built
        workingDigest="d-1"
        onPulled={onPulled}
      />,
    );

    const pullButton = await screen.findByRole("button", {
      name: "Pull update",
    });
    expect(screen.getByText("v0.4.0 → v0.5.0")).toBeTruthy();
    const notesList = screen.getByRole("list", { name: "Incoming changes" });
    expect(
      within(notesList)
        .getAllByRole("listitem")
        .map((el) => el.textContent),
    ).toEqual(["Add price alerts", "Fix CSV export"]);

    fireEvent.click(pullButton);

    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("pull_project_upstream", {
        id: "p-1",
        mode: "fastForward",
      }),
    );
    expect(onPulled).toHaveBeenCalledTimes(1);
    expect(
      await screen.findByText(
        /Pulled latest changes \(def5678\)\. Choose Update above to make it live\./,
      ),
    ).toBeTruthy();
  });

  it("protects local commits on diverged repos and confirms Replace with upstream", async () => {
    const onPulled = vi.fn();
    const diverged: UpstreamStatus = {
      ...UPDATE_AVAILABLE,
      state: "diverged",
      ahead: 2,
      behind: 1,
      localDirty: false,
    };
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      configurable: true,
    });

    let replaced = false;
    invokeMock.mockImplementation(async (command) => {
      if (command === "check_project_upstream") {
        return replaced
          ? ({
              ...diverged,
              state: "upToDate",
              ahead: 0,
              behind: 0,
              localCommit: "def5678",
              notes: [],
            } satisfies UpstreamStatus)
          : diverged;
      }
      if (command === "pull_project_upstream") {
        replaced = true;
        return {
          previousCommit: "abc1234",
          currentCommit: "def5678",
          backupBranch: "pockets-backup-abc1234",
          updated: true,
        } satisfies UpstreamPullResult;
      }
      throw new Error(`Unexpected command: ${command}`);
    });

    render(
      <UpstreamUpdates
        projectId="p-1"
        built
        workingDigest="d-1"
        onPulled={onPulled}
      />,
    );

    const copyBtn = await screen.findByRole("button", {
      name: "Copy merge prompt",
    });
    fireEvent.click(copyBtn);
    await waitFor(() => expect(writeText).toHaveBeenCalledTimes(1));

    fireEvent.click(
      screen.getByRole("button", { name: "Replace with upstream…" }),
    );
    const confirmBox = screen.getByRole("group", {
      name: "Confirm replace with upstream",
    });
    expect(confirmBox.textContent).toContain("pockets-backup-abc1234");

    fireEvent.click(
      within(confirmBox).getByRole("button", { name: "Replace with upstream" }),
    );
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("pull_project_upstream", {
        id: "p-1",
        mode: "replaceWithUpstream",
      }),
    );
    expect(onPulled).toHaveBeenCalledTimes(1);
    expect(await screen.findByText(/pockets-backup-abc1234/)).toBeTruthy();
  });

  it("hides Replace with upstream when the folder has uncommitted edits", async () => {
    invokeMock.mockResolvedValue({
      ...UPDATE_AVAILABLE,
      state: "diverged",
      ahead: 0,
      behind: 1,
      localDirty: true,
    } satisfies UpstreamStatus);

    render(<UpstreamUpdates projectId="p-1" built workingDigest="d-1" />);

    expect(
      await screen.findByRole("button", { name: "Copy merge prompt" }),
    ).toBeTruthy();
    expect(
      screen.queryByRole("button", { name: /Replace with upstream/ }),
    ).toBeNull();
    expect(
      screen.getByText(/Commit or stash your uncommitted edits first/),
    ).toBeTruthy();
  });

  it("stays collapsed when up to date and supports manual Check for updates", async () => {
    invokeMock.mockResolvedValue({
      ...UPDATE_AVAILABLE,
      state: "upToDate",
      behind: 0,
      notes: [],
    } satisfies UpstreamStatus);

    render(<UpstreamUpdates projectId="p-1" built workingDigest="d-1" />);

    const summary = await screen.findByText("Updates");
    const details = summary.closest("details")!;
    expect(details.open).toBe(false);

    details.open = true;
    fireEvent(details, new Event("toggle"));
    const checkBtn = screen.getByRole("button", { name: "Check for updates" });
    fireEvent.click(checkBtn);

    await waitFor(() =>
      expect(invokeMock).toHaveBeenLastCalledWith("check_project_upstream", {
        id: "p-1",
        force: true,
      }),
    );
  });
});
