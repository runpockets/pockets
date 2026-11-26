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
import { formatVersionTag } from "./formatVersion";
import Versions, {
  formatVersionTime,
  lineTotals,
  restoreSubject,
  versionSubject,
  summarizeChanges,
  type FileChange,
  type VersionsReport,
} from "./Versions";

vi.mock("./api", () => ({ invoke: vi.fn() }));

const invokeMock = vi.mocked(invoke);

const change = (
  path: string,
  kind: FileChange["kind"],
  addedLines: number | null = 1,
  removedLines: number | null = 0,
): FileChange => ({ path, kind, addedLines, removedLines });

const NOW = new Date(2026, 10, 23, 20, 30);
const seconds = (date: Date) => Math.floor(date.getTime() / 1000);

const REPORT: VersionsReport = {
  versions: [
    {
      revisionId: "c".repeat(32),
      installedAtSeconds: seconds(new Date(2026, 10, 23, 20, 1)),
      live: true,
      first: false,
      files: 4,
      notes: [],
      changes: [
        change("public/app.js", "modified", 30, 6),
        change("public/styles.css", "modified", 12, 2),
      ],
    },
    {
      revisionId: "b".repeat(32),
      installedAtSeconds: seconds(new Date(2026, 10, 22, 16, 40)),
      live: false,
      first: false,
      files: 4,
      notes: [],
      changes: [
        change("public/settings.js", "added", 81, 0),
        change("public/index.html", "modified", 7, 3),
      ],
    },
    {
      revisionId: "a".repeat(32),
      installedAtSeconds: seconds(new Date(2026, 10, 18, 15, 2)),
      live: false,
      first: true,
      files: 11,
      notes: [],
      changes: [],
    },
  ],
  pending: {
    changes: [
      change("server.js", "modified", 10, 3),
      change("logo.png", "added", null, null),
    ],
    notes: [],
  },
  sourceControl: "untracked",
};

function openVersions() {
  const summary = screen.getByText("Versions");
  const details = summary.closest("details")!;
  details.open = true;
  fireEvent(details, new Event("toggle"));
  return details;
}

beforeEach(() => {
  invokeMock.mockReset();
});

afterEach(() => {
  cleanup();
});

describe("summarizeChanges", () => {
  it("writes a commit-style line grouped by kind", () => {
    expect(
      summarizeChanges([
        change("public/settings.js", "added"),
        change("public/index.html", "modified"),
      ]),
    ).toBe("Add settings.js, update index.html");
    expect(
      summarizeChanges([
        change("a.js", "modified"),
        change("b.js", "modified"),
        change("c.js", "modified"),
        change("d.js", "modified"),
        change("old.css", "removed"),
      ]),
    ).toBe("Update a.js, b.js and 2 more, remove old.css");
    expect(summarizeChanges([change("server.js", "modified")])).toBe(
      "Update server.js",
    );
  });

  it("keeps the folder when two changed files share a name", () => {
    expect(
      summarizeChanges([
        change("api/index.js", "modified"),
        change("web/index.js", "modified"),
      ]),
    ).toBe("Update api/index.js and web/index.js");
  });
});

describe("lineTotals", () => {
  it("sums text files and skips binary ones", () => {
    expect(
      lineTotals([
        change("a.js", "modified", 3, 1),
        change("b.png", "added", null, null),
      ]),
    ).toEqual({ added: 3, removed: 1 });
    expect(lineTotals([change("b.png", "added", null, null)])).toBeNull();
  });
});

describe("formatVersionTime", () => {
  it("names today and yesterday and dates older versions", () => {
    expect(
      formatVersionTime(seconds(new Date(2026, 10, 23, 8, 1)), NOW),
    ).toMatch(/^Today, /);
    expect(
      formatVersionTime(seconds(new Date(2026, 10, 22, 16, 40)), NOW),
    ).toMatch(/^Yesterday, /);
    const older = formatVersionTime(
      seconds(new Date(2026, 10, 18, 15, 2)),
      NOW,
    );
    expect(older).not.toMatch(/^(Today|Yesterday)/);
    expect(older).toContain("18");
    expect(older).not.toContain("2026");
    expect(
      formatVersionTime(seconds(new Date(2025, 0, 5, 9, 0)), NOW),
    ).toContain("2025");
  });
});

describe("Versions", () => {
  it("fetches only once opened and lists versions newest first", async () => {
    invokeMock.mockResolvedValue(REPORT);
    render(
      <Versions
        projectId="p-1"
        workingDigest="w"
        installedDigest="i"
        liveSince={REPORT.versions[0].installedAtSeconds}
      />,
    );
    expect(screen.getByText(/^Live since /)).toBeTruthy();
    expect(invokeMock).not.toHaveBeenCalled();

    openVersions();
    expect(invokeMock).toHaveBeenCalledWith("list_project_versions", {
      id: "p-1",
    });
    const list = await screen.findByRole("list", { name: "Versions" });
    const items = within(list).getAllByRole("listitem");
    expect(items).toHaveLength(3);
    expect(items[0].textContent).toContain("Update app.js and styles.css");
    expect(within(items[0]).getByText("Live")).toBeTruthy();
    expect(items[0].textContent).toContain("2 files");
    expect(items[0].textContent).toContain("+42");
    expect(items[0].textContent).toContain("−8");
    expect(items[1].textContent).toContain(
      "Add settings.js, update index.html",
    );
    expect(items[1].textContent).toContain("2 files");
    expect(within(items[1]).queryByText("Live")).toBeNull();
    expect(items[2].textContent).toContain("First version · 11 files");
    expect(items[2].querySelector(".versions-meta")?.textContent).not.toContain(
      "11 files",
    );
    // The first version has nothing to compare with and is not restorable here,
    // so it stays a non-expandable row.
    expect(within(items[2]).queryByRole("button")).toBeNull();
    expect(items[2].querySelector(".versions-files")).toBeNull();
  });

  it("expands a version into its changed files", async () => {
    invokeMock.mockResolvedValue(REPORT);
    render(<Versions projectId="p-1" workingDigest="w" installedDigest="i" />);
    openVersions();
    const row = await screen.findByRole("button", {
      name: /Add settings\.js, update index\.html/,
    });
    expect(row.getAttribute("aria-expanded")).toBe("false");
    fireEvent.click(row);
    expect(row.getAttribute("aria-expanded")).toBe("true");
    const files = document.getElementById(row.getAttribute("aria-controls")!)!;
    expect(files.textContent).toContain("public/settings.js");
    expect(files.textContent).toContain("+81");
    expect(within(files).getByText("Added")).toBeTruthy();
    fireEvent.click(row);
    expect(row.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByText("public/settings.js")).toBeNull();
  });

  it("shows synced changes that are not live yet, with binary files uncounted", async () => {
    invokeMock.mockResolvedValue(REPORT);
    render(<Versions projectId="p-1" workingDigest="w" installedDigest="i" />);
    openVersions();
    const pending = await screen.findByText("Not live yet");
    const row = pending.closest(".versions-pending")!;
    expect(row.textContent).toContain("2 files changed");
    expect(row.textContent).toContain("+10");
    expect(row.textContent).toContain("−3");
    expect(row.textContent).toContain("Update to make it live");
  });

  it("suggests git only for a linked folder that is not tracked", async () => {
    invokeMock.mockResolvedValue(REPORT);
    const { unmount } = render(
      <Versions projectId="p-1" workingDigest="w" installedDigest="i" />,
    );
    openVersions();
    expect(await screen.findByText(/git init/)).toBeTruthy();
    unmount();

    for (const sourceControl of ["git", "unlinked"] as const) {
      invokeMock.mockResolvedValue({ ...REPORT, sourceControl });
      const view = render(
        <Versions projectId="p-1" workingDigest="w" installedDigest="i" />,
      );
      openVersions();
      await screen.findByRole("list", { name: "Versions" });
      expect(screen.queryByText(/git init/)).toBeNull();
      view.unmount();
    }
  });

  it("says when nothing has been made live yet", async () => {
    invokeMock.mockResolvedValue({
      versions: [],
      pending: null,
      sourceControl: "git",
    } satisfies VersionsReport);
    render(<Versions projectId="p-1" workingDigest="w" installedDigest="i" />);
    openVersions();
    expect(await screen.findByText(/No versions yet/)).toBeTruthy();
  });

  it("reports a failed read and retries", async () => {
    invokeMock.mockRejectedValueOnce("the archive is corrupt");
    render(<Versions projectId="p-1" workingDigest="w" installedDigest="i" />);
    openVersions();
    expect(
      await screen.findByText(
        /Could not load versions\. the archive is corrupt/,
      ),
    ).toBeTruthy();
    invokeMock.mockResolvedValueOnce(REPORT);
    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByRole("list", { name: "Versions" })).toBeTruthy();
    expect(invokeMock).toHaveBeenCalledTimes(2);
  });

  it("refetches the open list when an Update changes the installed source", async () => {
    invokeMock.mockResolvedValue(REPORT);
    const { rerender } = render(
      <Versions projectId="p-1" workingDigest="w" installedDigest="i" />,
    );
    openVersions();
    await screen.findByRole("list", { name: "Versions" });
    rerender(
      <Versions projectId="p-1" workingDigest="w" installedDigest="j" />,
    );
    await waitFor(() => expect(invokeMock).toHaveBeenCalledTimes(2));
  });
});

describe("Versions restore", () => {
  const GIT_REPORT: VersionsReport = { ...REPORT, sourceControl: "git" };

  function mockRestore(result: unknown) {
    invokeMock.mockImplementation(async (command) => {
      if (command === "list_project_versions") return GIT_REPORT;
      if (command === "restore_project_version") {
        if (result instanceof Error) throw result.message;
        return result;
      }
      throw new Error(`Unexpected command: ${command}`);
    });
  }

  it("offers Restore only in a folder tracked with git", async () => {
    for (const sourceControl of ["untracked", "unlinked"] as const) {
      invokeMock.mockResolvedValue({ ...REPORT, sourceControl });
      const view = render(
        <Versions projectId="p-1" workingDigest="w" installedDigest="i" />,
      );
      openVersions();
      const list = await screen.findByRole("list", { name: "Versions" });
      for (const item of within(list).getAllByRole("listitem")) {
        const [toggle] = within(item).queryAllByRole("button");
        if (toggle) fireEvent.click(toggle);
        expect(
          within(item).queryByRole("button", { name: /^Restore/ }),
        ).toBeNull();
      }
      view.unmount();
    }
  });

  it("offers the live version only once the folder has moved on from it", async () => {
    invokeMock.mockResolvedValue({ ...GIT_REPORT, pending: null });
    const view = render(
      <Versions projectId="p-1" workingDigest="w" installedDigest="i" />,
    );
    openVersions();
    const items = within(
      await screen.findByRole("list", { name: "Versions" }),
    ).getAllByRole("listitem");
    // Restore sits inside an opened row, beside the note action.
    const restoreIn = (item: HTMLElement) => {
      fireEvent.click(within(item).getAllByRole("button")[0]);
      return within(item).queryByRole("button", { name: /^Restore/ });
    };
    expect(restoreIn(items[0])).toBeNull();
    expect(restoreIn(items[1])).toBeTruthy();
    expect(restoreIn(items[2])).toBeTruthy();
    view.unmount();

    invokeMock.mockResolvedValue(GIT_REPORT);
    render(<Versions projectId="p-1" workingDigest="w" installedDigest="i" />);
    openVersions();
    const live = within(
      await screen.findByRole("list", { name: "Versions" }),
    ).getAllByRole("listitem")[0];
    fireEvent.click(within(live).getAllByRole("button")[0]);
    expect(within(live).getByRole("button", { name: /^Restore/ })).toBeTruthy();
  });

  it("restores after confirmation, commits, and asks the owner to refetch", async () => {
    mockRestore({ commit: "abc1234" });
    const onRestored = vi.fn();
    render(
      <Versions
        projectId="p-1"
        workingDigest="w"
        installedDigest="i"
        onRestored={onRestored}
      />,
    );
    openVersions();
    fireEvent.click(
      await screen.findByRole("button", {
        name: /^Add settings\.js, update index\.html/,
      }),
    );
    fireEvent.click(
      screen.getByRole("button", {
        name: "Restore Add settings.js, update index.html",
      }),
    );
    // Asking is not restoring.
    expect(
      invokeMock.mock.calls.some(([cmd]) => cmd === "restore_project_version"),
    ).toBe(false);
    const confirm = screen.getByRole("group", { name: "Confirm restore" });
    expect(confirm.textContent).toContain("commits that as a new change");
    fireEvent.click(within(confirm).getByRole("button", { name: "Restore" }));

    expect(
      await screen.findByText(/committed as abc1234\. Choose Update/),
    ).toBeTruthy();
    expect(invokeMock).toHaveBeenCalledWith("restore_project_version", {
      id: "p-1",
      revisionId: "b".repeat(32),
      subject: restoreSubject(REPORT.versions[1].installedAtSeconds),
    });
    expect(onRestored).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("group", { name: "Confirm restore" })).toBeNull();
  });

  it("cancels without restoring and reports a refused restore", async () => {
    mockRestore(new Error("Your folder has uncommitted changes."));
    const onRestored = vi.fn();
    render(
      <Versions
        projectId="p-1"
        workingDigest="w"
        installedDigest="i"
        onRestored={onRestored}
      />,
    );
    openVersions();
    fireEvent.click(
      await screen.findByRole("button", {
        name: /^Add settings\.js, update index\.html/,
      }),
    );
    const ask = screen.getByRole("button", {
      name: "Restore Add settings.js, update index.html",
    });
    fireEvent.click(ask);
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(screen.queryByRole("group", { name: "Confirm restore" })).toBeNull();

    fireEvent.click(ask);
    fireEvent.click(
      within(screen.getByRole("group", { name: "Confirm restore" })).getByRole(
        "button",
        { name: "Restore" },
      ),
    );
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("uncommitted changes");
    expect(onRestored).not.toHaveBeenCalled();
  });

  it("disables Restore while another operation is busy", async () => {
    mockRestore({ commit: "abc1234" });
    render(
      <Versions
        projectId="p-1"
        workingDigest="w"
        installedDigest="i"
        disabled
      />,
    );
    openVersions();
    fireEvent.click(
      await screen.findByRole("button", {
        name: /^Add settings\.js, update index\.html/,
      }),
    );
    const ask = screen.getByRole("button", {
      name: "Restore Add settings.js, update index.html",
    });
    expect(ask.hasAttribute("disabled")).toBe(true);
  });

  it("names the restored version by its absolute date", () => {
    const subject = restoreSubject(seconds(new Date(2026, 10, 23, 20, 1)));
    expect(subject).toMatch(/^Restore the version made live on /);
    expect(subject).toContain("2026");
    expect(subject).not.toContain("Today");
  });
});

describe("Version notes", () => {
  const base = REPORT.versions[1];

  it("names a version by the agent's notes, then its files", () => {
    expect(versionSubject(base)).toBe("Add settings.js, update index.html");
    expect(versionSubject({ ...base, notes: ["Add a settings page"] })).toBe(
      "Add a settings page",
    );
    expect(
      versionSubject({
        ...base,
        notes: ["Add a settings page", "Fix the header"],
      }),
    ).toBe("Add a settings page and 1 more change");
    expect(versionSubject(REPORT.versions[2])).toBe("First version · 11 files");
  });

  it("lists every agent note when the row opens, and pending notes too", async () => {
    invokeMock.mockResolvedValue({
      ...REPORT,
      versions: [
        { ...REPORT.versions[0], notes: ["Add filters", "Sort by price"] },
        ...REPORT.versions.slice(1),
      ],
      pending: { ...REPORT.pending!, notes: ["Fix rounding"] },
    } satisfies VersionsReport);
    render(<Versions projectId="p-1" workingDigest="w" installedDigest="i" />);
    openVersions();
    const pending = (await screen.findByText("Not live yet")).closest(
      ".versions-pending",
    )!;
    expect(pending.textContent).toContain("Fix rounding");

    const row = screen.getByRole("button", {
      name: /Add filters and 1 more change/,
    });
    fireEvent.click(row);
    const notes = screen.getAllByRole("list", { name: "Notes" }).at(-1)!;
    expect(
      within(notes)
        .getAllByRole("listitem")
        .map((item) => item.textContent),
    ).toEqual(["Add filters", "Sort by price"]);
  });

  it("shows file count on the metadata line for a first version with notes", async () => {
    invokeMock.mockResolvedValue({
      ...REPORT,
      versions: [
        {
          ...REPORT.versions[2],
          live: true,
          notes: ["Initial launch"],
        },
      ],
      pending: null,
    } satisfies VersionsReport);
    render(<Versions projectId="p-1" workingDigest="w" installedDigest="i" />);
    openVersions();
    const list = await screen.findByRole("list", { name: "Versions" });
    const item = within(list).getByRole("listitem");
    expect(item.querySelector(".versions-meta")?.textContent).toContain(
      "11 files",
    );
  });

  it("shows milestone manifestVersion tags without doubling leading v", async () => {
    expect(formatVersionTag("0.2.0")).toBe("v0.2.0");
    expect(formatVersionTag("v1.0.0")).toBe("v1.0.0");
    expect(formatVersionTag("V2.1.0")).toBe("v2.1.0");
    expect(formatVersionTag("  0.3.0  ")).toBe("v0.3.0");

    invokeMock.mockResolvedValue({
      ...REPORT,
      versions: [
        { ...REPORT.versions[0], manifestVersion: "v0.2.0" },
        REPORT.versions[1],
        REPORT.versions[2],
      ],
      pending: { ...REPORT.pending!, manifestVersion: "0.3.0" },
    } satisfies VersionsReport);
    render(<Versions projectId="p-1" workingDigest="w" installedDigest="i" />);
    openVersions();

    const pending = (await screen.findByText("Not live yet")).closest(
      ".versions-pending",
    )!;
    expect(pending.querySelector(".versions-tag")?.textContent).toBe("v0.3.0");

    const list = screen.getByRole("list", { name: "Versions" });
    const items = within(list).getAllByRole("listitem");
    expect(items[0].querySelector(".versions-tag")?.textContent).toBe("v0.2.0");
    expect(items[1].querySelector(".versions-tag")).toBeNull();
    expect(items[2].querySelector(".versions-tag")).toBeNull();
  });
});
