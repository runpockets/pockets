import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import type { ReactElement } from "react";
import RevisionHistory, {
  type ProjectRevision,
  type RevisionHistoryProps,
} from "./RevisionHistory";
import { invoke } from "./api";

// The component only ever talks to native through `invoke`; the real module
// would fail outside Tauri, so mock the whole module.
vi.mock("./api", () => ({ invoke: vi.fn() }));

const invokeMock = vi.mocked(invoke);

const WORKING = `sha256:${"a".repeat(64)}`;
const INSTALLED = `sha256:${"b".repeat(64)}`;
const OTHER = `sha256:${"c".repeat(64)}`;

/** Fixtures shaped like the native `list_project_revisions` payload, newest
 *  first — the order native sends is the order the panel must show. */
const REV_NEWER: ProjectRevision = {
  id: "r-2",
  createdAtSeconds: 1730000000,
  label: "chat: fix the header",
  sourceDigest: WORKING,
  totalBytes: 2048,
  files: 3,
};
const REV_OLDER: ProjectRevision = {
  id: "r-1",
  createdAtSeconds: 1729900000,
  label: "initial import",
  sourceDigest: INSTALLED,
  totalBytes: 1024,
  files: 2,
};
const REVISIONS: ProjectRevision[] = [REV_NEWER, REV_OLDER];

const CONFIRM_COPY =
  "Restoring changes the working source and preview. The installed app and saved data stay unchanged until Update.";

/** Only ever resolves list calls; any other command fails the test loudly. */
function listOnly(revisions: ProjectRevision[] = REVISIONS) {
  invokeMock.mockImplementation(async (cmd: string) => {
    if (cmd === "list_project_revisions") return revisions;
    throw new Error(`Unexpected command: ${cmd}`);
  });
}

function renderHistory(overrides: Partial<RevisionHistoryProps> = {}): {
  container: HTMLElement;
  props: RevisionHistoryProps;
  rerender: (ui: ReactElement) => void;
} {
  const props: RevisionHistoryProps = {
    projectId: "p-1",
    workingDigest: WORKING,
    installedDigest: INSTALLED,
    onRestored: vi.fn(),
    ...overrides,
  };
  const result = render(<RevisionHistory {...props} />);
  return {
    container: result.container,
    props,
    rerender: result.rerender,
  };
}

function revisionRows(container: HTMLElement): HTMLElement[] {
  return Array.from(container.querySelectorAll(".revision-item"));
}

async function openHistory(container: HTMLElement) {
  const summary = container.querySelector("summary");
  expect(summary).not.toBeNull();
  fireEvent.click(summary as Element);
  await screen.findByText("initial import");
}

beforeEach(() => {
  invokeMock.mockReset();
});

afterEach(() => {
  cleanup();
});

describe("RevisionHistory loading", () => {
  it("fetches lazily on open and refetches on digest changes only while open", async () => {
    listOnly();
    const { container, props, rerender } = renderHistory();

    // Closed: nothing has been invoked — history is never prefetched.
    expect(invokeMock).not.toHaveBeenCalled();

    // A digest change while closed must not fetch either.
    rerender(<RevisionHistory {...props} workingDigest={OTHER} />);
    expect(invokeMock).not.toHaveBeenCalled();

    // Opening fetches exactly once, with only the opaque project id.
    await openHistory(container);
    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("list_project_revisions", {
      id: "p-1",
    });

    // While open, a native-side change (chat save / Update) moving either
    // digest refetches the list — no polling involved.
    rerender(<RevisionHistory {...props} installedDigest={OTHER} />);
    await waitFor(() => expect(invokeMock).toHaveBeenCalledTimes(2));
  });

  it("renders real metadata in native order with digest-matched badges", async () => {
    listOnly();
    const { container } = renderHistory();
    await openHistory(container);

    const rows = revisionRows(container);
    // Displayed in the native newest-first order, never re-sorted.
    expect(
      rows.map((row) => row.querySelector(".revision-label")?.textContent),
    ).toEqual(["chat: fix the header", "initial import"]);

    // Actual native timestamps and snapshot size, not "now" placeholders.
    expect(rows[0].textContent).toContain(
      new Date(REV_NEWER.createdAtSeconds * 1000).toLocaleString(),
    );
    expect(rows[0].textContent).toContain("3 files · 2.0 KiB");
    expect(rows[1].textContent).toContain(
      new Date(REV_OLDER.createdAtSeconds * 1000).toLocaleString(),
    );
    expect(rows[1].textContent).toContain("2 files · 1.0 KiB");

    // Badges come purely from digest matching against the props.
    expect(rows[0].querySelector(".revision-badge-working")).not.toBeNull();
    expect(rows[0].querySelector(".revision-badge-installed")).toBeNull();
    expect(rows[1].querySelector(".revision-badge-installed")).not.toBeNull();
    expect(rows[1].querySelector(".revision-badge-working")).toBeNull();
  });

  it("shows an honest error with a retry that refetches", async () => {
    let listCalls = 0;
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_project_revisions") {
        listCalls += 1;
        if (listCalls === 1) throw new Error("native was busy");
        return REVISIONS;
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });

    const { container } = renderHistory();
    const summary = container.querySelector("summary") as Element;
    fireEvent.click(summary);

    expect(
      await screen.findByText(/Could not load source history/),
    ).toBeTruthy();
    expect(screen.getByText(/native was busy/)).toBeTruthy();
    expect(screen.queryByText("initial import")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Retry" }));
    await screen.findByText("initial import");
    expect(listCalls).toBe(2);
  });
});

describe("RevisionHistory restore", () => {
  it("confirms inline, invokes with exact ids and reports success plus warning", async () => {
    const onRestored = vi.fn();
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_project_revisions") return REVISIONS;
      if (cmd === "restore_project_revision")
        return {
          project: { id: "p-1" },
          previewWarning: "the preview needs a manual reload",
        };
      throw new Error(`Unexpected command: ${cmd}`);
    });

    const { container } = renderHistory({ onRestored });
    await openHistory(container);

    // Older revision (installed, not the current working source): the click
    // only opens the inline confirmation — no native call happens yet.
    const older = revisionRows(container)[1];
    fireEvent.click(within(older).getByRole("button", { name: "Restore" }));
    expect(screen.getByText(CONFIRM_COPY)).toBeTruthy();
    expect(invokeMock).not.toHaveBeenCalledWith(
      "restore_project_revision",
      expect.anything(),
    );

    fireEvent.click(
      within(older).getByRole("button", { name: "Confirm restore" }),
    );

    // Exactly the opaque ids cross to native.
    await waitFor(() => expect(onRestored).toHaveBeenCalledTimes(1));
    expect(invokeMock).toHaveBeenCalledWith("restore_project_revision", {
      id: "p-1",
      revisionId: "r-1",
    });

    // Success copy — and the preview warning shown beside it, never as a
    // failed restore.
    expect(
      await screen.findByText(
        "Source restored. Update to apply it to the installed app.",
      ),
    ).toBeTruthy();
    expect(
      screen.getByText(/Preview note: the preview needs a manual reload/),
    ).toBeTruthy();
    expect(screen.queryByText(/failed/i)).toBeNull();
  });

  it("guards restores: current revision disabled, disabled blocks confirm, cancel never invokes", async () => {
    listOnly();
    const { container, props, rerender } = renderHistory();
    await openHistory(container);

    // The revision matching the working digest is current: disabled button,
    // a click opens nothing and invokes nothing.
    const currentRow = revisionRows(container)[0];
    const currentButton = within(currentRow).getByRole("button", {
      name: "Restore",
    });
    expect(currentButton.getAttribute("disabled")).not.toBeNull();
    fireEvent.click(currentButton);
    expect(screen.queryByText(CONFIRM_COPY)).toBeNull();

    // Open the confirmation on the older revision, then have the owner flip
    // `disabled` (another operation went busy): the confirm is blocked.
    const older = revisionRows(container)[1];
    fireEvent.click(within(older).getByRole("button", { name: "Restore" }));
    expect(screen.getByText(CONFIRM_COPY)).toBeTruthy();

    rerender(<RevisionHistory {...props} disabled={true} />);
    const confirmButton = within(
      screen.getByRole("group", { name: "Confirm restore" }),
    ).getByRole("button", { name: "Confirm restore" });
    expect(confirmButton.getAttribute("disabled")).not.toBeNull();
    fireEvent.click(confirmButton);
    expect(invokeMock).not.toHaveBeenCalledWith(
      "restore_project_revision",
      expect.anything(),
    );

    // Back to idle: Cancel closes the confirmation without any native call.
    rerender(<RevisionHistory {...props} disabled={false} />);
    fireEvent.click(
      within(screen.getByRole("group", { name: "Confirm restore" })).getByRole(
        "button",
        { name: "Cancel" },
      ),
    );
    expect(screen.queryByText(CONFIRM_COPY)).toBeNull();
    expect(invokeMock).not.toHaveBeenCalledWith(
      "restore_project_revision",
      expect.anything(),
    );
  });
});
