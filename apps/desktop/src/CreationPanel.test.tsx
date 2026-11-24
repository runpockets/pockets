import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { useState } from "react";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import CreationPanel, {
  displayAddress,
  formatBuiltDate,
  formatCapturedAt,
  formatCapturedFull,
  type PocketCatalog,
  type Project,
} from "./CreationPanel";
import creationCss from "./CreationPanel.css?raw";
import { invoke, listen } from "./api";

// The panel only ever talks to the native backend through `invoke` and
// `listen`; the real modules would fail outside Tauri, so mock them.
vi.mock("./api", () => ({ invoke: vi.fn(), listen: vi.fn() }));

const invokeMock = vi.mocked(invoke);
const listenMock = vi.mocked(listen);

/** Opens a card's ⋯ menu and returns its Delete item (Delete lives in the
 *  menu so the card's action row never wraps). */
function deleteMenuItem(scope: HTMLElement): HTMLElement {
  fireEvent.click(within(scope).getByRole("button", { name: "More actions" }));
  return within(scope).getByRole("menuitem", { name: "Delete pocket…" });
}

// The source-history widget is the sibling module's contract; mock the whole
// module so these tests stay about the catalog and a restore can be triggered
// deterministically without opening real revision lists. The stub records the
// props every card passes and counts restores per mount inside its own state,
// so a remount (which would destroy in-flight builds and dialogs) shows up as
// a reset counter — in-place refreshes keep counting up.
const historyProps = vi.hoisted(() => vi.fn());
vi.mock("./RevisionHistory", () => ({
  default: ({
    projectId,
    workingDigest,
    installedDigest,
    onRestored,
    disabled,
  }: {
    projectId: string;
    workingDigest: string;
    installedDigest?: string | null;
    onRestored: () => void;
    disabled?: boolean;
  }) => {
    const [restores, setRestores] = useState(0);
    historyProps({ projectId, workingDigest, installedDigest, disabled });
    return (
      <button
        type="button"
        disabled={disabled}
        onClick={() => {
          setRestores((count) => count + 1);
          onRestored();
        }}
      >
        mock restore {projectId} {restores}
      </button>
    );
  },
}));

/**
 * Containerfile fixture that *looks* like markup. The technical-details
 * section must show it as literal text (React text child), never as live HTML.
 */
const CONTAINERFILE = [
  "FROM node:22-slim",
  '<img src="x" onerror="alert(\'pwned\')" />',
  "COPY . /app",
].join("\n");

/** Draft record shaped like the native `list_projects` payload. */
const DRAFT = {
  id: "p-1",
  manifest: {
    metadata: {
      id: "p-1",
      name: "Notes Pocket",
      version: "1.0.0",
      description: "A small notes app.",
    },
    runtime: { image: "node:22-slim", architectures: ["arm64"] },
    web: { port: 3000, healthcheck: "/healthz" },
    storage: { name: "notes-data", path: "/data" },
  },
  phase: "draft",
  builtImageId: null,
  previewed: false,
  files: ["Containerfile", "package.json", "src/server.ts"],
  totalBytes: 2048,
  containerfile: CONTAINERFILE,
  sourceDigest: `sha256:${"a".repeat(64)}`,
};

/** Installed record shaped like the native `list_projects` payload. */
const INSTALLED = {
  ...DRAFT,
  id: "p-2",
  manifest: {
    ...DRAFT.manifest,
    metadata: {
      ...DRAFT.manifest.metadata,
      id: "p-2",
      name: "Timer Pocket",
      description: "Counts down seconds.",
    },
  },
  phase: "installed",
  builtImageId: `sha256:${"c".repeat(64)}`,
};

beforeEach(() => {
  invokeMock.mockReset();
  historyProps.mockClear();
});

// Tailscale, Data, Source history and Technical details live only on the
// Manage view's card, which CreationPanel portals into a host element App
// supplies. Tests stand in for that host.
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
  vi.useRealTimers();
});

describe("CreationPanel import flow", () => {
  it("builds from the visible development workspace with the catalog hidden", async () => {
    const draft: Project = {
      ...DRAFT,
      phase: "draft",
      developmentLinked: true,
    };
    let completeBuild!: (project: unknown) => void;
    invokeMock.mockImplementation(async (command) => {
      if (command === "list_projects") return [draft];
      if (command === "pocket_thumb") return null;
      if (command === "project_build_status")
        return {
          state: "running",
          log: "Packaging assets",
          truncated: false,
          error: null,
        };
      if (command === "build_project")
        return new Promise((resolve) => {
          completeBuild = resolve;
        });
      throw new Error(`Unexpected command: ${command}`);
    });
    function AppFixture() {
      const [host, setHost] = useState<HTMLDivElement | null>(null);
      return (
        <>
          <div ref={setHost} role="region" aria-label="Development controls" />
          <div hidden>
            <CreationPanel
              runtimeReady
              page="drafts"
              workspace={host ? { project: draft, host } : undefined}
            />
          </div>
        </>
      );
    }
    render(<AppFixture />);
    const workspaceCard = screen.getByRole("region", {
      name: "Development controls",
    });
    const workspace = within(workspaceCard);
    const build = workspace.getByRole("button", { name: "Build and start" });
    await waitFor(() => expect(build.hasAttribute("disabled")).toBe(false));
    fireEvent.click(build);
    expect(invokeMock).toHaveBeenCalledWith("build_project", { id: draft.id });
    fireEvent.click(
      workspace.getByRole("button", { name: "Technical details" }),
    );
    expect(await workspace.findByText("Packaging assets")).toBeTruthy();
    // A linked folder is the source, so Pockets' snapshot restore is not
    // offered there: undo happens in git or the coding tool, then Update.
    expect(
      workspace.queryByRole("button", { name: /mock restore/ }),
    ).toBeNull();
    expect(deleteMenuItem(workspaceCard).hasAttribute("disabled")).toBe(true);
    await act(async () => completeBuild(draft));
  });

  it("offers Update in the workspace for an installed linked Pocket even before manual sync", async () => {
    const installedLinked: Project = {
      ...INSTALLED,
      phase: "installed",
      developmentLinked: true,
      installed: {
        imageId: INSTALLED.builtImageId!,
        manifest: INSTALLED.manifest,
        sourceDigest: INSTALLED.sourceDigest,
      },
    };
    invokeMock.mockImplementation(async (command) => {
      if (command === "list_projects") return [installedLinked];
      if (command === "pocket_thumb") return null;
      if (command === "project_status")
        return {
          state: "running",
          url: "http://timer.localhost:18788",
          message: "Running",
          ready: true,
        };
      if (command === "project_build_status")
        return { state: "succeeded", log: "", truncated: false, error: null };
      if (command === "build_project") return installedLinked;
      throw new Error(`Unexpected command: ${command}`);
    });
    function AppFixture() {
      const [host, setHost] = useState<HTMLDivElement | null>(null);
      return (
        <>
          <div ref={setHost} role="region" aria-label="Development controls" />
          <div hidden>
            <CreationPanel
              runtimeReady
              page="live"
              workspace={host ? { project: installedLinked, host } : undefined}
            />
          </div>
        </>
      );
    }
    render(<AppFixture />);
    const workspace = within(
      screen.getByRole("region", { name: "Development controls" }),
    );
    const update = await workspace.findByRole("button", { name: "Update" });
    await waitFor(() => expect(update.hasAttribute("disabled")).toBe(false));
    expect(update.className).not.toContain("button-primary");
    fireEvent.click(update);
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("build_project", {
        id: installedLinked.id,
      }),
    );
  });

  it("highlights unsynced linked-folder changes and promotes Update to primary", async () => {
    const editedLinked: Project = {
      ...INSTALLED,
      phase: "installed",
      developmentLinked: true,
      unsyncedChanges: true,
      installed: {
        imageId: INSTALLED.builtImageId!,
        manifest: INSTALLED.manifest,
        sourceDigest: INSTALLED.sourceDigest,
      },
    };
    invokeMock.mockImplementation(async (command) => {
      if (command === "list_projects") return [editedLinked];
      if (command === "pocket_thumb") return null;
      if (command === "project_status")
        return {
          state: "running",
          url: "http://timer.localhost:18788",
          message: "Running",
          ready: true,
        };
      throw new Error(`Unexpected command: ${command}`);
    });
    render(<CreationPanel runtimeReady page="live" />);
    const card = await screen.findByRole("article", { name: "Timer Pocket" });
    expect(within(card).getByText("Unsynced changes")).toBeTruthy();
    expect(
      within(card).getByText(
        /Your linked folder has changes not yet in the installed app/,
      ),
    ).toBeTruthy();
    const update = within(card).getByRole("button", { name: "Update" });
    expect(update.className).toContain("button-primary");
  });

  it("links a development folder through the native picker and refreshes without building", async () => {
    let connected = false;
    invokeMock.mockImplementation(async (command) => {
      if (command === "list_projects") return connected ? [DRAFT] : [];
      if (command === "link_development_folder") {
        connected = true;
        return DRAFT;
      }
      if (command === "pocket_thumb") return null;
      throw new Error(`Unexpected command: ${command}`);
    });
    render(<CreationPanel runtimeReady={false} page="drafts" />);
    await screen.findByText(/No drafts yet/);
    fireEvent.click(screen.getByRole("button", { name: "Link project" }));
    expect(await screen.findByText("Notes Pocket")).toBeTruthy();
    expect(invokeMock).toHaveBeenCalledWith("link_development_folder", {
      id: null,
    });
    expect(
      invokeMock.mock.calls.some(
        ([command]) =>
          command === "build_project" || command === "start_project",
      ),
    ).toBe(false);
  });

  it("starts the native picker without a path and treats cancel as no-op", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_projects") return [];
      // Native owns the folder picker; `null` is the user cancelling it.
      // Only the import picker stays nullable — builds never cancel.
      if (cmd === "import_project") return null;
      throw new Error(`Unexpected command: ${cmd}`);
    });

    // An import lands a draft record, so the picker lives in reach of the
    // page that lists drafts.
    render(<CreationPanel runtimeReady={true} page="drafts" />);
    await screen.findByText(/No drafts yet/);

    // v2 renamed the toolbar action "Add created Pocket" → "Import pocket";
    // the picker flow itself is unchanged.
    fireEvent.click(screen.getByRole("button", { name: "Import pocket" }));

    // Exact argument list: the webview never sends a path to native.
    expect(invokeMock).toHaveBeenCalledWith("import_project");

    // A cancel is not a failure: still the empty state, no error text, no
    // phantom cards.
    expect(await screen.findByText(/No drafts yet/)).toBeTruthy();
    expect(screen.queryByText(/import failed/i)).toBeNull();
    expect(screen.queryByText(/Could not load/i)).toBeNull();
    expect(
      screen.queryByRole("button", { name: "Technical details" }),
    ).toBeNull();
  });
});

describe("CreationPanel technical details", () => {
  it("toggles the optional snapshot details with the Containerfile escaped", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_projects") return [DRAFT];
      throw new Error(`Unexpected command: ${cmd}`);
    });

    // The one draft record is a Drafts-page card; its Technical details live
    // on the Manage view's card only.
    const host = manageHost();
    const { container } = render(
      <CreationPanel
        runtimeReady={true}
        page="drafts"
        workspace={{ project: DRAFT as Project, host }}
      />,
    );
    const grid = within(container);
    await grid.findByRole("heading", { name: "Notes Pocket" });
    expect(
      grid.queryByRole("button", { name: "Technical details" }),
    ).toBeNull();
    expect(container.querySelector(".creation-card-disclosures")).toBeNull();

    const manage = within(host);
    // A draft has never been made live, so it has no Versions yet.
    expect(manage.queryByText("Versions")).toBeNull();

    // The Build and start button lives in its own primary action row outside
    // and above the collapsed disclosures, so expanding Technical details
    // reveals its content directly beneath its own toggle.
    const build = manage.getByRole("button", { name: "Build and start" });
    const toggle = manage.getByRole("button", { name: "Technical details" });
    expect(build.closest(".creation-card-actions")).toBeTruthy();
    expect(toggle.closest(".creation-card-actions")).toBeNull();
    expect(
      Boolean(
        build.compareDocumentPosition(toggle) &
        Node.DOCUMENT_POSITION_FOLLOWING,
      ),
    ).toBe(true);
    expect(host.querySelector(".creation-containerfile")).toBeNull();
    expect(
      manage.queryByText(/generated Containerfile instructions/),
    ).toBeNull();

    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(toggle.nextElementSibling?.className).toBe("creation-review");

    // Notes explaining build and snapshot semantics are visible.
    expect(
      manage.getByText(/generated Containerfile instructions/),
    ).toBeTruthy();

    // The markup-like Containerfile renders as literal text only.
    const pre = host.querySelector(".creation-containerfile");
    expect(pre).not.toBeNull();
    expect(pre?.textContent).toBe(CONTAINERFILE);
    expect(host.querySelector("img")).toBeNull();
    expect(host.querySelector("script")).toBeNull();

    // File inventory lists the snapshot paths.
    const listed = Array.from(
      host.querySelectorAll(".creation-files li code"),
    ).map((node) => node.textContent);
    expect(listed).toEqual(DRAFT.files);

    // Collapsing hides the whole details section again — but never the
    // Build and start button.
    fireEvent.click(toggle);
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(host.querySelector(".creation-containerfile")).toBeNull();
    expect(
      manage.getByRole("button", { name: "Build and start" }),
    ).toBeTruthy();
  });
});

describe("CreationPanel build retry", () => {
  /** Manually-settled promise so a native command can stay deferred. */
  function deferred() {
    let resolve!: (value: unknown) => void;
    let reject!: (reason?: unknown) => void;
    const promise = new Promise<unknown>((res, rej) => {
      resolve = res;
      reject = rej;
    });
    return { promise, resolve, reject };
  }

  it("surfaces new running progress after retrying a failed build", async () => {
    // Only the 1s build-log interval is virtual; findBy*/waitFor* keep real
    // timeouts, and the test advances ticks explicitly inside act().
    vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });

    // Latest status native would report through `project_build_status`.
    // Native contract: until the new run is actually underway the previous
    // run's terminal status is still reported, so a retry initially still
    // reads the old `failed`.
    let nativeStatus: unknown = {
      state: "failed",
      log: "Step 2/8: pulling the base image",
      truncated: false,
      error: "exit status 1",
    };
    let projectsList: unknown[] = [DRAFT];
    const firstBuild = deferred();
    const retryBuild = deferred();
    let buildCalls = 0;

    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_projects") return projectsList;
      if (cmd === "project_build_status") return nativeStatus;
      if (cmd === "project_status")
        return {
          state: "running",
          url: "http://127.0.0.1:3000",
          message: "Answering HTTP health checks.",
          ready: true,
        };
      if (cmd === "build_project") {
        buildCalls += 1;
        return buildCalls === 1 ? firstBuild.promise : retryBuild.promise;
      }
      // A fresh draft's one click builds AND starts, so the successful retry
      // chains the usual id-only open. They answer like native; the failed
      // first attempt must never reach them.
      if (cmd === "start_project")
        return {
          state: "running",
          url: "http://127.0.0.1:3000",
          message: "Answering HTTP health checks.",
          ready: true,
        };
      if (cmd === "open_project") return undefined;
      throw new Error(`Unexpected command: ${cmd}`);
    });

    // The build log is read on the Manage view's card (Technical details);
    // the build is pressed on the Drafts card.
    const host = manageHost();
    const { container, unmount } = render(
      <CreationPanel
        runtimeReady={true}
        page="drafts"
        onNewPocket={vi.fn()}
        workspace={{ project: DRAFT as Project, host }}
      />,
    );
    const grid = within(container);
    const manage = within(host);
    await grid.findByRole("heading", { name: "Notes Pocket" });
    fireEvent.click(manage.getByRole("button", { name: "Technical details" }));

    // First attempt fails: reject the build promise while native already
    // reports the failed status. Pressing Build and start is itself the
    // approval — there is no second confirmation step.
    fireEvent.click(grid.getByRole("button", { name: "Build and start" }));
    await act(async () => {
      firstBuild.reject(new Error("exit status 1"));
    });
    expect(await grid.findByText(/The build failed/)).toBeTruthy();
    expect(manage.getByText("Step 2/8: pulling the base image")).toBeTruthy();
    // A failed build never opens: nothing was installed, so the chained
    // start/open must not have been reached.
    expect(
      invokeMock.mock.calls.some(
        (call) => call[0] === "start_project" || call[0] === "open_project",
      ),
    ).toBe(false);

    let retrySettled = false;
    retryBuild.promise.then(
      () => {
        retrySettled = true;
      },
      () => {
        retrySettled = true;
      },
    );

    // Retry: the failed draft offers "Retry build" outside the details. Press
    // is the approval itself, so the run starts straight in a packaging
    // state — never an approval-waiting state — while the promise stays
    // deferred and native still reports the previous failed status.
    fireEvent.click(grid.getByRole("button", { name: "Retry build" }));
    expect(screen.getAllByText(/Packaging/i).length).toBeGreaterThan(0);
    expect(screen.queryByText(/approval/i)).toBeNull();

    // Let this run's first status read consume the stale `failed` snapshot.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });

    // Regression (part 1): the stale terminal status must neither surface as
    // this run's failure nor stop polling.
    expect(screen.queryByText(/The build failed/)).toBeNull();
    expect(screen.queryByText(/Step 4\/8/)).toBeNull();
    expect(screen.getAllByText(/Packaging/i).length).toBeGreaterThan(0);

    // Behind the still-pending promise, native has moved on to the new build.
    nativeStatus = {
      state: "running",
      log: "Step 4/8: installing dependencies for the retried build",
      truncated: false,
      error: null,
    };
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });

    // Regression (part 2): the retry's new running log has to reach the panel
    // while the build promise is still in flight.
    expect(await manage.findByText(/Step 4\/8/)).toBeTruthy();
    expect(grid.getByText(/Building your Pocket/)).toBeTruthy();
    expect(screen.queryByText(/The build failed/)).toBeNull();
    expect(retrySettled).toBe(false);

    // Settle the retry normally so no promise or interval leaks past cleanup.
    // Success means installed: native persists the image id and the installed
    // phase in one step, with no Keep step.
    await act(async () => {
      nativeStatus = {
        state: "succeeded",
        log: "Step 8/8: wrote the image",
        truncated: false,
        error: null,
      };
      projectsList = [
        {
          ...DRAFT,
          phase: "installed",
          builtImageId: `sha256:${"b".repeat(64)}`,
        },
      ];
      retryBuild.resolve(projectsList[0]);
      await vi.advanceTimersByTimeAsync(0);
    });

    // A successful build auto-saves on the native side, and the refetch is
    // what reports it: this Drafts page loses the card entirely — its grid
    // keeps nothing but the dashed new-pocket cell — and the Build and start
    // affordance is gone with it, because there is no draft left here.
    await waitFor(() =>
      expect(container.querySelector(".creation-card")).toBeNull(),
    );
    expect(grid.queryByRole("article", { name: "Notes Pocket" })).toBeNull();
    expect(grid.queryByRole("button", { name: "Build and start" })).toBeNull();
    expect(screen.getByText(/No drafts yet/)).toBeTruthy();
    const cells = Array.from(container.querySelectorAll(".creation-grid > li"));
    expect(cells).toHaveLength(1);
    expect(cells[0].querySelector(".creation-new")).toBeTruthy();

    // And one click is still one action: the retried build did not stop at
    // installing, it chained the same id-only open the Open button uses —
    // without a card left on this page to press anything.
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("open_project", { id: "p-1" }),
    );

    // The promotion is native's, not a splice this component made: a fresh
    // render of the page that lists installed records reads the same list and
    // shows the Pocket there, with no build path left on it.
    unmount();
    render(<CreationPanel runtimeReady={true} page="live" />);
    expect(screen.getByRole("heading", { name: "Live pockets" })).toBeTruthy();
    const built = await screen.findByRole("article", { name: "Notes Pocket" });
    expect(
      within(built).queryByRole("button", { name: "Build and start" }),
    ).toBeNull();
  });
});

describe("CreationPanel build autosave and open", () => {
  it("auto-saves a successful build as Installed and opens with id-only commands", async () => {
    // Authoritative persisted record the native mock mutates in place, like
    // real storage would: a successful build persists the image id and the
    // installed phase in one write, still unpreviewed. No Keep step exists.
    const record = { ...DRAFT, builtImageId: null as string | null };
    const site = "http://notes-pocket.localhost:18788";
    let nativeRunning = false;

    invokeMock.mockImplementation(async (cmd: string) => {
      switch (cmd) {
        case "list_projects":
          return [{ ...record }];
        case "build_project": {
          if (record.phase !== "draft" || record.builtImageId !== null) {
            throw new Error("build_project must only be pressed on a draft");
          }
          // Never null: on success native returns the installed record.
          record.builtImageId = `sha256:${"c".repeat(64)}`;
          record.phase = "installed";
          return { ...record };
        }
        case "project_build_status":
          return {
            state: "succeeded",
            log: "Step 8/8: wrote the image",
            truncated: false,
            error: null,
          };
        case "project_status":
          return nativeRunning
            ? {
                state: "running",
                url: site,
                message: "Answering HTTP health checks.",
                ready: true,
              }
            : {
                state: "stopped",
                url: null,
                message: "Not running.",
                ready: false,
              };
        case "start_project":
          nativeRunning = true;
          return {
            state: "running",
            url: site,
            message: "Answering HTTP health checks.",
            ready: true,
          };
        case "open_project":
          // Opening only starts and browses; no preview marker drives any
          // affordance anymore.
          return undefined;
        default:
          throw new Error(`Unexpected command: ${cmd}`);
      }
    });

    const { container, rerender } = render(
      <CreationPanel runtimeReady={true} page="drafts" onNewPocket={vi.fn()} />,
    );
    await screen.findByRole("heading", { name: "Notes Pocket" });

    // The unexpanded draft offers Build and start — no Approve button, and no
    // second Open to go hunting for on another page — and it works without
    // ever opening the optional technical details.
    expect(
      screen.getByRole("button", { name: "Build and start" }),
    ).toBeTruthy();
    expect(container.querySelector(".creation-containerfile")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Build and start" }));

    // Auto-save: success promotes the record to Installed on its own, and the
    // refetched list is what says so — the card leaves this page's grid,
    // which is left with nothing but the dashed new-pocket cell. There is no
    // `.creation-badge` anywhere.
    await waitFor(() =>
      expect(container.querySelector(".creation-card")).toBeNull(),
    );
    expect(record.previewed).toBe(false);
    expect(container.querySelector(".creation-containerfile")).toBeNull();
    const cells = Array.from(container.querySelectorAll(".creation-grid > li"));
    expect(cells).toHaveLength(1);
    expect(cells[0].querySelector(".creation-new")).toBeTruthy();

    // One click, one action: with no card left on this page to press Open, the
    // build chained the open sequence itself, in the same order Open uses.
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("start_project", { id: "p-1" });
      expect(invokeMock).toHaveBeenCalledWith("open_project", { id: "p-1" });
    });
    const commands = invokeMock.mock.calls.map((call) => call[0]);
    expect(commands.indexOf("start_project")).toBeLessThan(
      commands.indexOf("open_project"),
    );
    // The Pocket really is up by the end of it — the mock's own state, not a
    // label the panel claimed.
    expect(nativeRunning).toBe(true);

    // No keep, preview, or confirmation command is ever invoked: Build and
    // start is the single approval and saving is automatic.
    expect(
      commands.some((command) => /keep|preview|confirm/i.test(command)),
    ).toBe(false);
    // And nothing but ids ever crosses to native — no url, path, or image
    // leaves the webview on any command this flow ran (the thumbnail reads key
    // on `projectId`, which is an id too).
    for (const [, args] of invokeMock.mock.calls) {
      const keys = Object.keys((args ?? {}) as Record<string, unknown>);
      expect(keys.filter((key) => key !== "id" && key !== "projectId")).toEqual(
        [],
      );
    }
    rerender(
      <CreationPanel runtimeReady={true} page="live" onNewPocket={vi.fn()} />,
    );
    expect(
      await screen.findByText("notes-pocket.localhost:18788"),
    ).toBeTruthy();
  });

  /** A native command held open until the test hands it its outcome, so an
   *  operation can be observed while it is genuinely still running. */
  function gate() {
    let resolve!: (value: unknown) => void;
    let reject!: (reason?: unknown) => void;
    const promise = new Promise<unknown>((res, rej) => {
      resolve = res;
      reject = rej;
    });
    return { promise, resolve, reject };
  }

  /** The build-and-start native store with `start_project` and `open_project`
   *  held by the caller, so the chained open can be watched step by step.
   *  `openError` makes the open command reject instead, the way a real native
   *  failure does. */
  function mockBuildWithGatedOpen(
    start: Promise<unknown>,
    opened: Promise<unknown>,
    openError?: string,
  ) {
    const record = { ...DRAFT, builtImageId: null as string | null };
    invokeMock.mockImplementation(async (cmd: string) => {
      switch (cmd) {
        case "list_projects":
          return [{ ...record }];
        case "build_project":
          record.builtImageId = `sha256:${"c".repeat(64)}`;
          record.phase = "installed";
          return { ...record };
        case "project_build_status":
          return {
            state: "succeeded",
            log: "Step 8/8: wrote the image",
            truncated: false,
            error: null,
          };
        case "project_status":
          return {
            state: "stopped",
            url: null,
            message: "Not running.",
            ready: false,
          };
        case "start_project":
          return start;
        case "open_project":
          if (openError) throw new Error(openError);
          return opened;
        default:
          throw new Error(`Unexpected command: ${cmd}`);
      }
    });
    return record;
  }

  it("says the chained start and open at panel level once the card has left the drafts grid", async () => {
    const starting = gate();
    const opening = gate();
    const record = mockBuildWithGatedOpen(starting.promise, opening.promise);

    const { container } = render(
      <CreationPanel runtimeReady={true} page="drafts" onNewPocket={vi.fn()} />,
    );
    await screen.findByRole("heading", { name: "Notes Pocket" });
    fireEvent.click(screen.getByRole("button", { name: "Build and start" }));

    // The build promoted the record, so this page has no card left to carry
    // the progress. The panel says it itself — the card's own word, the
    // pocket's real name, and the same health-check note.
    const status = () =>
      container.querySelector(".creation-status")?.textContent ?? "";
    await waitFor(() => expect(status()).toContain("Starting Notes Pocket…"));
    expect(status()).toContain(
      "waiting for the Pocket's HTTP health check, which can take about 30 seconds",
    );
    // Said once: no card on this page repeats it, and the line is the drafts
    // page's own, not a second grid.
    expect(container.querySelectorAll(".creation-card")).toHaveLength(0);
    expect(screen.getAllByText(/Starting Notes Pocket…/)).toHaveLength(1);
    // And the panel is still busy — the click is not over just because the
    // card that carried it is gone from this page.
    expect(
      container.querySelector(".creation-panel")?.getAttribute("aria-busy"),
    ).toBe("true");

    // Start lands, native is still opening the browser: the line moves with
    // the operation instead of going quiet on a Pocket nothing has opened yet.
    await act(async () => {
      starting.resolve({
        state: "running",
        url: "http://127.0.0.1:3000",
        message: "Answering HTTP health checks.",
        ready: true,
      });
    });
    expect(status()).toContain("Opening Notes Pocket…");
    expect(status()).not.toContain("Starting Notes Pocket…");

    await act(async () => {
      opening.resolve(undefined);
    });
    // The finished operation leaves no stale claim behind, and the record is
    // the installed one the refetch reported.
    await waitFor(() => expect(status()).not.toContain("Notes Pocket"));
    expect(record.phase).toBe("installed");
  });

  it("says a failed chained open honestly instead of pretending nothing happened", async () => {
    mockBuildWithGatedOpen(
      Promise.resolve({
        state: "running",
        url: "http://127.0.0.1:3000",
        message: "Answering HTTP health checks.",
        ready: true,
      }),
      Promise.resolve(undefined),
      "The browser refused the address.",
    );

    const { container } = render(
      <CreationPanel runtimeReady={true} page="drafts" onNewPocket={vi.fn()} />,
    );
    await screen.findByRole("heading", { name: "Notes Pocket" });
    fireEvent.click(screen.getByRole("button", { name: "Build and start" }));

    const status = () =>
      container.querySelector(".creation-status")?.textContent ?? "";
    // The install really happened and the open really did not, and the line
    // says both — plus where the Pocket can be reached, because this page no
    // longer lists it.
    await waitFor(() =>
      expect(status()).toContain(
        "Notes Pocket is built and installed, but it did not open.",
      ),
    );
    expect(status()).toContain("The browser refused the address.");
    expect(status()).toContain("Live pockets page");
    // No auto-retry and no rebuild: the flow ran open once on the record the
    // build already promoted, and the failure is the end of the click.
    const commands = invokeMock.mock.calls.map((call) => call[0]);
    expect(
      commands.filter((command) => command === "open_project"),
    ).toHaveLength(1);
    expect(
      commands.filter((command) => command === "build_project"),
    ).toHaveLength(1);
  });
});

describe("CreationPanel delete flow", () => {
  /** Renders the Drafts page holding only the DRAFT card and returns that
   *  card. */
  async function renderWithDraft(deleteImpl?: () => Promise<unknown>) {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_projects") return [DRAFT];
      if (cmd === "delete_project") {
        if (deleteImpl) return deleteImpl();
        return null;
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });
    render(<CreationPanel runtimeReady={true} page="drafts" />);
    return screen.findByRole("article", { name: "Notes Pocket" });
  }

  it("confirms a draft deletion with the Delete Pocket button alone", async () => {
    const card = await renderWithDraft();
    fireEvent.click(deleteMenuItem(card));

    const dialog = await screen.findByRole("dialog");
    // A draft's title says so: the word "draft" belongs to the pocket's
    // persisted phase, not the page it happens to be deleted from.
    expect(
      within(dialog).getByRole("heading", {
        name: "Delete “Notes Pocket” draft?",
      }),
    ).toBeTruthy();
    const confirm = within(dialog).getByRole("button", {
      name: "Delete Pocket",
    });

    // The ask alone must never reach native...
    expect(invokeMock).not.toHaveBeenCalledWith(
      "delete_project",
      expect.anything(),
    );
    // ...and a draft owes no typed name: no field is rendered and the
    // confirm button stands ready the moment the dialog opens.
    expect(within(dialog).queryByRole("textbox")).toBeNull();
    expect((confirm as HTMLButtonElement).disabled).toBe(false);
    // The warning is honest about what dies with an unbuilt draft: the
    // record and its snapshot, never a container or volume it never had.
    const warning = within(dialog).getByText(/permanently removes the draft/);
    expect(warning.textContent).toContain("record and its source snapshot");
    expect(warning.textContent).not.toContain("data volume");

    const listCallsBefore = invokeMock.mock.calls.filter(
      (c) => c[0] === "list_projects",
    ).length;
    fireEvent.click(confirm);

    // Exact argument list: only the id crosses to native.
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("delete_project", {
        id: "p-1",
      }),
    );
    // The modal closes and the list refetches: native stays the only source
    // of what exists — the card never disappears optimistically.
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(
      invokeMock.mock.calls.filter((c) => c[0] === "list_projects").length,
    ).toBeGreaterThan(listCallsBefore);
  });

  /** Renders the Live pockets page holding only the INSTALLED card. */
  async function renderWithInstalled() {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_projects") return [INSTALLED];
      if (cmd === "delete_project") return null;
      throw new Error(`Unexpected command: ${cmd}`);
    });
    render(<CreationPanel runtimeReady={true} page="live" />);
    return screen.findByRole("article", { name: "Timer Pocket" });
  }

  it("still requires the exact typed name for a live pocket", async () => {
    const card = await renderWithInstalled();
    fireEvent.click(deleteMenuItem(card));

    const dialog = await screen.findByRole("dialog");
    const confirm = within(dialog).getByRole("button", {
      name: "Delete Pocket",
    });
    const input = within(dialog).getByLabelText(/Type the Pocket/);

    // The installed warning keeps naming everything delete_project removes,
    // and the installed title claims no "draft".
    expect(within(dialog).getByText(/data volume/)).toBeTruthy();
    expect(
      within(dialog).getByRole("heading", { name: "Delete “Timer Pocket”?" }),
    ).toBeTruthy();

    // The gate stays exactly the GitHub repo-deletion pattern for an
    // installed record: empty or partial names cannot confirm, and nothing
    // reaches native before the button is clickable.
    expect((confirm as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(input, { target: { value: "Timer" } });
    expect((confirm as HTMLButtonElement).disabled).toBe(true);
    expect(invokeMock).not.toHaveBeenCalledWith(
      "delete_project",
      expect.anything(),
    );

    fireEvent.change(input, { target: { value: "Timer Pocket" } });
    expect((confirm as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(confirm);

    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("delete_project", {
        id: "p-2",
      }),
    );
  });

  it("keeps the dialog open with the native reason when the delete fails", async () => {
    const card = await renderWithDraft(async () => {
      throw new Error("The container refused to be removed.");
    });
    fireEvent.click(deleteMenuItem(card));
    const dialog = await screen.findByRole("dialog");
    fireEvent.click(
      within(dialog).getByRole("button", { name: "Delete Pocket" }),
    );

    const error = await within(dialog).findByText(/refused to be removed/);
    expect(error).toBeTruthy();
    // Still open and retryable: the draft confirm needs nothing to re-enter.
    expect(screen.queryByRole("dialog")).toBeTruthy();
    expect(
      (
        within(dialog).getByRole("button", {
          name: "Delete Pocket",
        }) as HTMLButtonElement
      ).disabled,
    ).toBe(false);
  });

  it("Escape closes the confirmation without invoking anything", async () => {
    const card = await renderWithDraft();
    fireEvent.click(deleteMenuItem(card));
    await screen.findByRole("dialog");

    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(invokeMock).not.toHaveBeenCalledWith(
      "delete_project",
      expect.anything(),
    );
  });
});

describe("CreationPanel catalog publication", () => {
  /** Manually-settled promise so the build can stay in flight. */
  function deferred() {
    let resolve!: (value: unknown) => void;
    const promise = new Promise<unknown>((res) => {
      resolve = res;
    });
    return { promise, resolve };
  }

  it("tracks the native store through build, lifecycle, delete and import", async () => {
    // Mutable native-style store: commands mutate it and every list refetch
    // reads it back, mirroring native as the only source of what exists.
    const record = { ...DRAFT, builtImageId: null as string | null };
    let store: Record<string, unknown>[] = [record];
    let running = false;
    const build = deferred();

    invokeMock.mockImplementation(async (cmd: string) => {
      switch (cmd) {
        case "list_projects":
          return store.map((entry) => ({ ...entry }));
        case "project_build_status":
          return { state: "succeeded", log: "", truncated: false, error: null };
        case "build_project":
          // Gate the build so the running publish can be observed.
          return build.promise;
        case "project_status":
          return running
            ? {
                state: "running",
                url: "http://127.0.0.1:3000",
                message: "Answering HTTP health checks.",
                ready: true,
              }
            : {
                state: "stopped",
                url: null,
                message: "Not running.",
                ready: false,
              };
        case "start_project":
          running = true;
          return {
            state: "running",
            url: "http://127.0.0.1:3000",
            message: "Answering HTTP health checks.",
            ready: true,
          };
        case "open_project":
          return undefined;
        case "delete_project":
          store = [];
          return null;
        case "import_project": {
          const imported = {
            ...DRAFT,
            id: "p-9",
            manifest: {
              ...DRAFT.manifest,
              metadata: {
                ...DRAFT.manifest.metadata,
                id: "p-9",
                name: "Imported Pocket",
              },
            },
          };
          store = [{ ...imported }];
          return { ...imported };
        }
        default:
          throw new Error(`Unexpected command: ${cmd}`);
      }
    });

    const catalogs: PocketCatalog[] = [];
    const { rerender } = render(
      <CreationPanel
        runtimeReady={true}
        page="drafts"
        onCatalogChange={(catalog) => catalogs.push(catalog)}
      />,
    );
    const latest = () => catalogs[catalogs.length - 1];
    /** Moves the one mounted panel to the other page the way the App's route
     *  does — no remount, and the published catalog is the whole store either
     *  way. `page` only decides which records the grid renders, so a card is
     *  clickable only on the page its `phase` names. */
    const showPage = (page: "live" | "drafts") =>
      rerender(
        <CreationPanel
          runtimeReady={true}
          page={page}
          onCatalogChange={(catalog) => catalogs.push(catalog)}
        />,
      );

    // The first publish is the honest initial snapshot: nothing loaded yet.
    expect(catalogs[0]).toEqual({
      projects: [],
      statuses: {},
      loading: true,
      error: null,
      buildingProjectId: null,
    });

    // Loaded: the draft appears with no status entry — drafts have no
    // lifecycle, and the catalog never fabricates one.
    await waitFor(() =>
      expect(latest()).toEqual({
        projects: [expect.objectContaining({ id: "p-1", phase: "draft" })],
        statuses: {},
        loading: false,
        error: null,
        buildingProjectId: null,
      }),
    );

    // Build in flight: the running build's id is published.
    fireEvent.click(screen.getByRole("button", { name: "Build and start" }));
    expect(latest().buildingProjectId).toBe("p-1");

    // Build settles: the record promotes to installed, and the same click goes
    // on to start and open it — so the status the catalog ends up carrying is
    // the real running one, never a stopped snapshot of a Pocket that is up.
    await act(async () => {
      record.builtImageId = `sha256:${"c".repeat(64)}`;
      record.phase = "installed";
      build.resolve({ ...record });
    });
    await waitFor(() =>
      expect(latest()).toMatchObject({
        projects: [expect.objectContaining({ phase: "installed" })],
        statuses: {
          "p-1": {
            status: "ready",
            value: expect.objectContaining({ state: "running" }),
          },
        },
        buildingProjectId: null,
      }),
    );
    // The chained open is the id-only pair, in order, with no manual step.
    const commands = invokeMock.mock.calls.map((call) => call[0]);
    expect(commands.indexOf("start_project")).toBeGreaterThan(-1);
    expect(commands.indexOf("start_project")).toBeLessThan(
      commands.indexOf("open_project"),
    );

    // Delete: the card's removal comes from the refetched list, and the
    // published catalog drops both the project and its stale status. The
    // promoted record is installed now, so its card is on the live page — the
    // drafts page no longer lists it.
    showPage("live");
    fireEvent.click(deleteMenuItem(document.body));
    const dialog = await screen.findByRole("dialog");
    fireEvent.change(within(dialog).getByLabelText(/Type the Pocket/), {
      target: { value: "Notes Pocket" },
    });
    fireEvent.click(
      within(dialog).getByRole("button", { name: "Delete Pocket" }),
    );
    await waitFor(() =>
      expect(latest()).toEqual({
        projects: [],
        statuses: {},
        loading: false,
        error: null,
        buildingProjectId: null,
      }),
    );

    // Import: a confirmed native import reaches the catalog through the
    // refetch, like everything else. v2 renamed the toolbar button to
    // "Import pocket". The record native writes is a draft, so the panel is
    // moved back to the page that lists drafts.
    showPage("drafts");
    fireEvent.click(screen.getByRole("button", { name: "Import pocket" }));
    await waitFor(() =>
      expect(latest()).toMatchObject({
        projects: [expect.objectContaining({ id: "p-9", phase: "draft" })],
        loading: false,
      }),
    );
  });
});

describe("CreationPanel chat action", () => {
  /** Two cards: one draft, one built and stopped. */
  function mockCatalog() {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_projects") return [DRAFT, INSTALLED];
      if (cmd === "project_status")
        return {
          state: "stopped",
          url: null,
          message: "Not running.",
          ready: false,
        };
      throw new Error(`Unexpected command: ${cmd}`);
    });
  }

  it("renders chat on every card bound to that card's id", async () => {
    mockCatalog();
    const onSelectProject = vi.fn();
    const { rerender } = render(
      <CreationPanel
        runtimeReady={true}
        page="drafts"
        onSelectProject={onSelectProject}
      />,
    );

    const draft = await screen.findByRole("article", { name: "Notes Pocket" });

    // The card's workspace navigation button carries the accessible name "Manage".
    fireEvent.click(within(draft).getByRole("button", { name: "Manage" }));
    expect(onSelectProject).toHaveBeenCalledTimes(1);
    expect(onSelectProject).toHaveBeenCalledWith("p-1");

    // The installed record's card is the same component on the other page,
    // chat included: the id follows the card, never the page, the click order
    // or the position.
    rerender(
      <CreationPanel
        runtimeReady={true}
        page="live"
        onSelectProject={onSelectProject}
      />,
    );
    const built = await screen.findByRole("article", { name: "Timer Pocket" });
    fireEvent.click(within(built).getByRole("button", { name: "Manage" }));
    expect(onSelectProject).toHaveBeenCalledTimes(2);
    expect(onSelectProject).toHaveBeenLastCalledWith("p-2");
  });

  it("omits chat entirely when the App passes no selection handler", async () => {
    mockCatalog();
    render(<CreationPanel runtimeReady={true} page="drafts" />);
    await screen.findByRole("article", { name: "Notes Pocket" });
    expect(screen.queryByRole("button", { name: "Manage" })).toBeNull();
  });
});

describe("CreationPanel new pocket cell", () => {
  /** One installed pocket and one draft: the two records live on their own
   *  pages, so a page's grid holds only what its route names. */
  function mockTwoRecords() {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_projects") return [INSTALLED, DRAFT];
      if (cmd === "project_status")
        return {
          state: "stopped",
          url: null,
          message: "Not running.",
          ready: false,
        };
      throw new Error(`Unexpected command: ${cmd}`);
    });
  }

  it("keeps the dashed new-pocket cell the last cell of the Drafts grid", async () => {
    mockTwoRecords();

    const onNewPocket = vi.fn();
    const { container } = render(
      <CreationPanel
        runtimeReady={true}
        page="drafts"
        onNewPocket={onNewPocket}
      />,
    );
    // v2's heading is a plain page title — the count lives in the subtitle.
    const title = await screen.findByRole("heading", { name: "Drafts" });
    expect(title.tagName).toBe("H2");
    expect(screen.getByRole("button", { name: "New pocket" })).toBeTruthy();

    // The dashed cell is the page's one grid's last cell — after the real
    // draft card — and the installed record is not rendered here at all
    // (handoff: the grid keeps the new-pocket cell, and a new pocket becomes
    // a draft).
    const grids = Array.from(container.querySelectorAll(".creation-grid"));
    expect(grids).toHaveLength(1);
    expect(grids[0].getAttribute("aria-labelledby")).toBe("creation-heading");
    const cells = Array.from(grids[0].querySelectorAll(":scope > li"));
    expect(cells).toHaveLength(2);
    expect(cells[0].querySelector(".creation-card")).toBeTruthy();
    expect(cells[1].querySelector(".creation-new")).toBeTruthy();
    expect(screen.queryByRole("article", { name: "Timer Pocket" })).toBeNull();

    // The dashed cell stays a real, fully labeled button...
    const cell = screen.getByRole("button", {
      name: /Start with a local source folder/,
    });

    // ...calling the callback only — a pure view action, never a native
    // command.
    fireEvent.click(cell);
    expect(onNewPocket).toHaveBeenCalledTimes(1);
    const commands = invokeMock.mock.calls.map((call) => call[0]);
    expect(
      commands.some((command) => /create|new|scaffold/i.test(command)),
    ).toBe(false);
  });

  it("keeps the cell with nothing to show, next to the honest empty line", async () => {
    // The old no-match case was a filter artifact; with two routed pages there
    // is exactly one empty state per page, and the new-pocket cell still
    // renders inside the drafts grid even with no draft at all (the header
    // button always did too).
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_projects") return [];
      throw new Error(`Unexpected command: ${cmd}`);
    });

    const { container } = render(
      <CreationPanel runtimeReady={true} page="drafts" onNewPocket={vi.fn()} />,
    );
    expect(await screen.findByText(/No drafts yet/)).toBeTruthy();
    const grids = container.querySelectorAll(".creation-grid");
    expect(grids).toHaveLength(1);
    const cells = Array.from(grids[0].querySelectorAll(":scope > li"));
    expect(cells).toHaveLength(1);
    expect(cells[0].querySelector(".creation-new")).toBeTruthy();
  });

  it("never draws the new-pocket cell on the Live pockets page", async () => {
    // The cell creates a draft, so the live page — installed records only —
    // never renders it, with or without records of its own. Its header button
    // is the shared way in and stays on both pages.
    mockTwoRecords();
    const { container } = render(
      <CreationPanel runtimeReady={true} page="live" onNewPocket={vi.fn()} />,
    );
    await screen.findByRole("article", { name: "Timer Pocket" });
    expect(container.querySelector(".creation-new")).toBeNull();
    expect(screen.getByRole("button", { name: "New pocket" })).toBeTruthy();
    expect(screen.queryByRole("article", { name: "Notes Pocket" })).toBeNull();

    // An empty live page is the same: the honest empty line and no grid at
    // all — the cell belongs to the other page even as the only way out.
    cleanup();
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_projects") return [];
      throw new Error(`Unexpected command: ${cmd}`);
    });
    const { container: emptyPage } = render(
      <CreationPanel runtimeReady={true} page="live" onNewPocket={vi.fn()} />,
    );
    expect(await screen.findByText(/No live Pockets yet/)).toBeTruthy();
    expect(emptyPage.querySelector(".creation-grid")).toBeNull();
    expect(emptyPage.querySelector(".creation-new")).toBeNull();
  });
});

describe("CreationPanel page modes", () => {
  /** Draft imported as a copy of `INSTALLED`: the native record carries the
   *  parent's id in `editedFrom`, the only thing the lineage caption is
   *  allowed to read. Ordinary fixtures above keep the field absent, as
   *  native does. */
  const LINEAGE_DRAFT = {
    ...DRAFT,
    id: "p-3",
    manifest: {
      ...DRAFT.manifest,
      metadata: {
        ...DRAFT.manifest.metadata,
        id: "p-3",
        name: "Notes Copy",
        description: "A copy of the running Timer Pocket.",
      },
    },
    editedFrom: INSTALLED.id,
  };

  /** Lineage toward a record this list does not contain (a deleted parent). */
  const ORPHAN_DRAFT = {
    ...LINEAGE_DRAFT,
    id: "p-4",
    manifest: {
      ...LINEAGE_DRAFT.manifest,
      metadata: {
        ...LINEAGE_DRAFT.manifest.metadata,
        id: "p-4",
        name: "Deleted Parent Copy",
      },
    },
    editedFrom: "p-deleted",
  };

  /** Lineage toward a record that exists but is itself a draft (`p-1`):
   *  only an installed record can be a parent, so this claims nothing. */
  const DRAFT_OF_DRAFT = {
    ...LINEAGE_DRAFT,
    id: "p-5",
    manifest: {
      ...LINEAGE_DRAFT.manifest,
      metadata: {
        ...LINEAGE_DRAFT.manifest.metadata,
        id: "p-5",
        name: "Draft Copy",
      },
    },
    editedFrom: DRAFT.id,
  };

  function mockProjects(...projects: Array<Record<string, unknown>>) {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_projects") return projects;
      if (cmd === "project_status")
        return {
          state: "stopped",
          url: null,
          message: "Not running.",
          ready: false,
        };
      throw new Error(`Unexpected command: ${cmd}`);
    });
  }

  /** The cards of the page's one grid, in reading order. */
  function gridCards(grid: Element): HTMLElement[] {
    return Array.from(grid.querySelectorAll<HTMLElement>(".creation-card"));
  }

  it("splits installed and draft records across the two pages in catalog order", async () => {
    // Catalog order p-1 (draft), p-2 (installed), p-3 (draft): the routes
    // split on the persisted `phase` and each keeps the list's own order —
    // nothing is re-sorted, and a record cannot be on both pages because
    // `phase` is a single persisted value.
    mockProjects(DRAFT, INSTALLED, LINEAGE_DRAFT);
    const onSelectProject = vi.fn();
    const { container, rerender } = render(
      <CreationPanel
        runtimeReady={true}
        page="drafts"
        onSelectProject={onSelectProject}
      />,
    );
    // The page's title is the h2 the grid is labelled by — there is no
    // section heading inside the page to label a second grid.
    const title = await screen.findByRole("heading", { name: "Drafts" });
    expect(title.tagName).toBe("H2");
    expect(title.id).toBe("creation-heading");
    expect(screen.queryByRole("heading", { name: "Live pockets" })).toBeNull();

    const grids = Array.from(container.querySelectorAll(".creation-grid"));
    expect(grids).toHaveLength(1);
    expect(grids[0].getAttribute("aria-labelledby")).toBe("creation-heading");
    expect(
      gridCards(grids[0]).map((card) => card.getAttribute("aria-labelledby")),
    ).toEqual(["creation-name-p-1", "creation-name-p-3"]);
    expect(screen.queryByRole("article", { name: "Timer Pocket" })).toBeNull();

    // Every card sits in a bare `<li>` — the same flat cell the single grid
    // always drew — with no wrapper class anywhere: the pile and group
    // machinery is gone.
    for (const cell of Array.from(grids[0].querySelectorAll(":scope > li"))) {
      expect(cell.className).toBe("");
      expect(cell.firstElementChild?.classList.contains("creation-card")).toBe(
        true,
      );
    }
    expect(container.querySelector(".creation-group")).toBeNull();
    expect(container.querySelector(".creation-pile")).toBeNull();

    // The drafts page hands its drafts over with their primary actions —
    // promotion path, Manage and delete, bound to the card's own id. Source
    // history and Technical details live on the Manage view only.
    const draft = screen.getByRole("article", { name: "Notes Pocket" });
    expect(
      within(draft).getByRole("button", { name: "Build and start" }),
    ).toBeTruthy();
    expect(deleteMenuItem(draft)).toBeTruthy();
    expect(
      within(draft).queryByRole("button", { name: /^mock restore p-1/ }),
    ).toBeNull();
    expect(
      within(draft).queryByRole("button", { name: "Technical details" }),
    ).toBeNull();
    fireEvent.click(within(draft).getByRole("button", { name: "Manage" }));
    expect(onSelectProject).toHaveBeenCalledTimes(1);
    expect(onSelectProject).toHaveBeenCalledWith("p-1");

    // The other route reads the same list and shows the record its phase
    // names, with the lifecycle it was actually read with. Moving a card
    // between pages never swaps it for a lesser variant.
    rerender(
      <CreationPanel
        runtimeReady={true}
        page="live"
        onSelectProject={onSelectProject}
      />,
    );
    const liveGrids = Array.from(container.querySelectorAll(".creation-grid"));
    expect(liveGrids).toHaveLength(1);
    expect(
      gridCards(liveGrids[0]).map((card) =>
        card.getAttribute("aria-labelledby"),
      ),
    ).toEqual(["creation-name-p-2"]);
    const built = await screen.findByRole("article", { name: "Timer Pocket" });
    await within(built).findByText("Stopped");
    expect(within(built).getByRole("button", { name: "Open" })).toBeTruthy();
    expect(
      within(built).queryByRole("button", { name: /^mock restore p-2/ }),
    ).toBeNull();
    expect(deleteMenuItem(built)).toBeTruthy();
    expect(screen.queryByRole("article", { name: "Notes Pocket" })).toBeNull();
  });

  it("captions a draft whose recorded lineage resolves to a listed installed record", async () => {
    mockProjects(INSTALLED, LINEAGE_DRAFT);
    const { container } = render(
      <CreationPanel runtimeReady={true} page="drafts" />,
    );
    const draft = await screen.findByRole("article", { name: "Notes Copy" });

    // The caption is the parent's real manifest name from this catalog,
    // stated once under the card title — never its id, never an invented
    // lifecycle word. The parent itself is an installed record, so it is not
    // rendered on this page at all: the caption resolves from the catalog,
    // not from the cards sharing the grid.
    const caption = within(draft).getByText("Edited from Timer Pocket");
    expect(caption.classList.contains("creation-edited-from")).toBe(true);
    expect(caption.textContent).not.toContain(INSTALLED.id);
    expect(screen.queryByRole("article", { name: "Timer Pocket" })).toBeNull();

    // The card stays the unmodified component — same single class, every
    // affordance, no grouping wrapper anywhere.
    expect(draft.className).toBe("creation-card");
    expect(
      within(draft).getByRole("button", { name: "Build and start" }),
    ).toBeTruthy();
    expect(deleteMenuItem(draft)).toBeTruthy();
    expect(container.querySelector(".creation-group")).toBeNull();
    expect(container.querySelector(".creation-pile")).toBeNull();
  });

  it("claims no lineage the catalog cannot resolve", async () => {
    // A draft without the field, a draft whose recorded id names a record
    // absent from this list (a deleted parent), a draft whose recorded id
    // names another draft, and a lineage draft whose installed parent is
    // simply not listed: none of these may draw a caption, and the raw
    // unresolvable ids never reach the page either.
    mockProjects(DRAFT, ORPHAN_DRAFT, DRAFT_OF_DRAFT, LINEAGE_DRAFT);
    const { container } = render(
      <CreationPanel runtimeReady={true} page="drafts" />,
    );
    await screen.findByRole("article", { name: "Notes Pocket" });

    expect(screen.queryByText(/Edited from/)).toBeNull();
    expect(container.querySelector(".creation-edited-from")).toBeNull();
    expect(document.body.textContent).not.toContain("p-deleted");
  });

  it("hands a record to the other page rather than drawing it twice", async () => {
    // A drafts-only catalog: the live page renders its own honest empty line
    // with no grid, while the record is on the drafts page. `phase` decides
    // the page, so there is no "section" to omit — the page is the split.
    mockProjects(DRAFT);
    const { container, rerender } = render(
      <CreationPanel runtimeReady={true} page="live" />,
    );
    expect(await screen.findByText(/No live Pockets yet/)).toBeTruthy();
    expect(container.querySelector(".creation-grid")).toBeNull();
    expect(screen.queryByRole("article", { name: "Notes Pocket" })).toBeNull();

    rerender(<CreationPanel runtimeReady={true} page="drafts" />);
    await screen.findByRole("article", { name: "Notes Pocket" });
    expect(container.querySelectorAll(".creation-grid")).toHaveLength(1);
    expect(screen.queryByText(/No live Pockets yet/)).toBeNull();
  });

  it("shows an installed-only catalog on the live page alone", async () => {
    mockProjects(INSTALLED);
    const { container, rerender } = render(
      <CreationPanel runtimeReady={true} page="drafts" onNewPocket={vi.fn()} />,
    );
    // The empty drafts page keeps the dashed cell and never leaks the
    // installed card here.
    expect(await screen.findByText(/No drafts yet/)).toBeTruthy();
    expect(container.querySelector(".creation-new")).toBeTruthy();
    expect(screen.queryByRole("article", { name: "Timer Pocket" })).toBeNull();

    rerender(<CreationPanel runtimeReady={true} page="live" />);
    await screen.findByRole("article", { name: "Timer Pocket" });
    expect(container.querySelectorAll(".creation-grid")).toHaveLength(1);
    expect(container.querySelector(".creation-new")).toBeNull();
    expect(screen.queryByText(/No drafts yet/)).toBeNull();
  });

  it("states each page's own empty copy with no subtitle", async () => {
    // Nothing to count, so nothing to claim: the subtitle is dropped and the
    // one empty line names the page's own way out — a live page is empty
    // because nothing has been built, a drafts page because nothing has been
    // created or imported.
    mockProjects();
    const { container, rerender } = render(
      <CreationPanel runtimeReady={true} page="live" />,
    );
    const live = await screen.findByText(/No live Pockets yet/);
    expect(live.textContent).toBe(
      "No live Pockets yet. Build a Pocket to see it here.",
    );
    expect(container.querySelector(".creation-subtitle")).toBeNull();
    expect(screen.queryByText(/No drafts yet/)).toBeNull();

    rerender(<CreationPanel runtimeReady={true} page="drafts" />);
    const drafts = await screen.findByText(/No drafts yet/);
    expect(drafts.textContent).toBe(
      "No drafts yet. Create a new Pocket, link a project, or import one.",
    );
    expect(container.querySelector(".creation-subtitle")).toBeNull();
    expect(screen.queryByText(/No live Pockets yet/)).toBeNull();
  });

  it("filters catalog cards by name or description and clears cleanly", async () => {
    const second = {
      ...INSTALLED,
      id: "p-3",
      manifest: {
        ...INSTALLED.manifest,
        metadata: {
          ...INSTALLED.manifest.metadata,
          id: "p-3",
          name: "Habit Tracker",
          description: "Daily streaks and routines.",
        },
      },
    };
    mockProjects(INSTALLED, second);
    render(<CreationPanel runtimeReady={false} page="live" />);
    await screen.findByRole("article", { name: "Timer Pocket" });
    const search = screen.getByRole("searchbox", {
      name: "Filter live pockets",
    });

    fireEvent.change(search, { target: { value: "streaks" } });
    expect(screen.queryByRole("article", { name: "Timer Pocket" })).toBeNull();
    expect(screen.getByRole("article", { name: "Habit Tracker" })).toBeTruthy();

    fireEvent.change(search, { target: { value: "nonexistent" } });
    expect(screen.getByText(/No Pockets match/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Clear filter" }));
    expect(screen.getByRole("article", { name: "Timer Pocket" })).toBeTruthy();
    expect(screen.getByRole("article", { name: "Habit Tracker" })).toBeTruthy();
  });

  it("calls out high CPU in amber next to the status", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_projects") return [INSTALLED];
      if (cmd === "pocket_thumb") return null;
      if (cmd === "project_status") {
        return {
          state: "running",
          ready: true,
          url: "http://pocket-p2.localhost:18789",
          message: "running",
        };
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });
    render(
      <CreationPanel
        runtimeReady={true}
        page="live"
        usageByProject={{
          [INSTALLED.id]: {
            id: INSTALLED.id,
            name: "Timer Pocket",
            state: "running",
            cpuPercent: 87.2,
            memoryBytes: 64 * 1024 * 1024,
          },
        }}
      />,
    );
    const card = await screen.findByRole("article", { name: "Timer Pocket" });
    const figure = await within(card).findByText(/^· CPU /);
    expect(figure.getAttribute("data-tone")).toBe("warn");
    // Memory moves into the tooltip only; it is not repeated beside CPU.
    expect(within(card).queryByText("· 64.0 MiB")).toBeNull();
    expect(
      within(within(card).getByRole("tooltip", { hidden: true })).getByText(
        "64.0 MiB",
      ),
    ).toBeTruthy();
  });

  it("shows measured CPU and memory on running live cards and omits them when stopped", async () => {
    const stopped = {
      ...INSTALLED,
      id: "p-3",
      manifest: {
        ...INSTALLED.manifest,
        metadata: {
          ...INSTALLED.manifest.metadata,
          id: "p-3",
          name: "Idle Pocket",
        },
      },
    };
    invokeMock.mockImplementation(async (cmd: string, args?: unknown) => {
      if (cmd === "list_projects") return [INSTALLED, stopped];
      if (cmd === "pocket_thumb") return null;
      if (cmd === "project_status") {
        const id = (args as { id: string }).id;
        return id === INSTALLED.id
          ? {
              state: "running",
              ready: true,
              url: "http://pocket-p2.localhost:18789",
              message: "running",
            }
          : { state: "stopped", ready: false, url: null, message: "stopped" };
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });
    render(
      <CreationPanel
        runtimeReady={true}
        page="live"
        usageByProject={{
          [INSTALLED.id]: {
            id: INSTALLED.id,
            name: "Timer Pocket",
            state: "running",
            cpuPercent: 2.4,
            memoryBytes: 64 * 1024 * 1024,
          },
          [stopped.id]: {
            id: stopped.id,
            name: "Idle Pocket",
            state: "stopped",
            cpuPercent: 0,
            memoryBytes: 0,
          },
        }}
      />,
    );

    const runningCard = await screen.findByRole("article", {
      name: "Timer Pocket",
    });
    expect(await within(runningCard).findByText("2.4%")).toBeTruthy();
    expect(within(runningCard).getByText("64.0 MiB")).toBeTruthy();
    // Calm usage: memory rides on the status in grey; the exact figures sit
    // in a tooltip the status opens on hover or keyboard focus.
    const figure = within(runningCard).getByText("· 64.0 MiB");
    expect(figure.getAttribute("data-tone")).toBeNull();
    const tooltip = within(runningCard).getByRole("tooltip", { hidden: true });
    expect(within(tooltip).getByText("2.4%")).toBeTruthy();
    const status = runningCard.querySelector(".creation-card-status")!;
    expect(status.getAttribute("tabindex")).toBe("0");
    expect(status.getAttribute("aria-describedby")).toBe(tooltip.id);

    const stoppedCard = screen.getByRole("article", { name: "Idle Pocket" });
    expect(stoppedCard.querySelector(".creation-usage")).toBeNull();
    expect(
      stoppedCard.querySelector(".creation-card-status-figure"),
    ).toBeNull();
    expect(
      stoppedCard
        .querySelector(".creation-card-status")
        ?.hasAttribute("tabindex"),
    ).toBe(false);

    // Grid cards stay lean: Tailscale, Data, Source history and Technical
    // details are reached through Manage, not repeated on every card.
    for (const card of [runningCard, stoppedCard]) {
      expect(card.querySelector(".creation-card-disclosures")).toBeNull();
      expect(within(card).queryByText("Data")).toBeNull();
      expect(within(card).queryByText("Tailscale")).toBeNull();
    }
  });

  it("exports a data backup and confirms before resetting a Pocket's Podman volume", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_projects") return [INSTALLED];
      if (cmd === "pocket_thumb") return null;
      if (cmd === "project_status") {
        return {
          state: "running",
          ready: true,
          url: "http://pocket-p2.localhost:18789",
          message: "running",
        };
      }
      if (cmd === "export_project_data") return "timer-pocket-data-backup.tar";
      if (cmd === "restore_project_data") {
        return {
          fileName: "timer-pocket-data-backup.tar",
          status: {
            state: "stopped",
            ready: false,
            url: null,
            message: "stopped",
          },
        };
      }
      if (cmd === "reset_project_data") {
        return {
          state: "stopped",
          ready: false,
          url: null,
          message: "stopped",
        };
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });

    // Data lives on the Manage view's card.
    const host = manageHost();
    const { container } = render(
      <CreationPanel
        runtimeReady={true}
        page="live"
        workspace={{ project: INSTALLED as Project, host }}
      />,
    );
    const card = await within(container).findByRole("article", {
      name: "Timer Pocket",
    });
    await within(card).findByText("Running");
    expect(within(card).queryByText("Data")).toBeNull();

    // An installed Pocket lists the versions made live; without a linked
    // folder its snapshot restore (Source history) stays available too.
    expect(within(host).getByText("Versions")).toBeTruthy();
    expect(
      within(host).getByRole("button", { name: /^mock restore p-2/ }),
    ).toBeTruthy();

    const dataDetails = within(host).getByText("Data").closest("details")!;
    expect(within(dataDetails).getByText("Local Podman volume")).toBeTruthy();
    dataDetails.open = true;
    fireEvent(dataDetails, new Event("toggle"));

    fireEvent.click(
      within(dataDetails).getByRole("button", { name: "Export backup…" }),
    );
    expect(
      await within(dataDetails).findByText(
        "Backup saved to timer-pocket-data-backup.tar.",
      ),
    ).toBeTruthy();
    expect(invokeMock).toHaveBeenCalledWith("export_project_data", {
      id: INSTALLED.id,
    });

    // Restore backup requires confirmation, invokes restore_project_data, and
    // updates status to Stopped.
    fireEvent.click(
      within(dataDetails).getByRole("button", { name: "Restore backup…" }),
    );
    const restoreDialog = screen.getByRole("dialog", {
      name: /Restore backup for “Timer Pocket”/,
    });
    expect(
      invokeMock.mock.calls.some(([cmd]) => cmd === "restore_project_data"),
    ).toBe(false);
    fireEvent.click(
      within(restoreDialog).getByRole("button", { name: "Choose backup…" }),
    );
    expect(
      await within(dataDetails).findByText(
        /Backup restored from timer-pocket-data-backup\.tar\./,
      ),
    ).toBeTruthy();
    expect(invokeMock).toHaveBeenCalledWith("restore_project_data", {
      id: INSTALLED.id,
    });

    // Reset data requires explicit confirmation and updates status to Stopped.
    fireEvent.click(
      within(dataDetails).getByRole("button", { name: "Reset data" }),
    );
    const dialog = screen.getByRole("dialog", {
      name: /Reset data for “Timer Pocket”/,
    });
    expect(
      invokeMock.mock.calls.some(([cmd]) => cmd === "reset_project_data"),
    ).toBe(false);
    fireEvent.click(within(dialog).getByRole("button", { name: "Reset data" }));

    expect(await within(dataDetails).findByText(/Data reset\./)).toBeTruthy();
    expect(invokeMock).toHaveBeenCalledWith("reset_project_data", {
      id: INSTALLED.id,
    });
    expect(within(card).getByText("Stopped")).toBeTruthy();
  });
});

/**
 * Card thumbnails: the panel reads one stored capture per listed project
 * (`pocket_thumb`, keyed by project id) and shows it under a caption that
 * names when the picture is *from*. Absence, failure and malformed answers
 * all render the plain card — no placeholder, no invented art, and never a
 * present-tense claim about the UI.
 */
describe("CreationPanel thumbnails", () => {
  // A fixed instant so the caption's expected text is computed, not guessed;
  // the formatting is the platform's own, same idiom as `formatBuiltDate`.
  const CAPTURED_AT = Date.UTC(2026, 10, 21, 20, 4, 5);
  const THUMB = {
    dataUrl: "data:image/png;base64,cG9ja2V0LXRodW1i",
    capturedAt: CAPTURED_AT,
  };

  const RUNNING = {
    state: "running",
    url: "http://127.0.0.1:7100",
    message: "Running.",
    ready: true,
  };

  function mockProjectsAndThumbs(
    projects: Array<Record<string, unknown>>,
    thumbFor: (projectId: string) => unknown,
    extra: {
      /** Answer for `project_status` (invoked with `{ id }`). Defaults to
       *  stopped, which is why the plain tests never trigger a capture. */
      status?: (id: string) => unknown;
      /** Answer for `pocket_thumb_capture` (invoked with `{ projectId }`).
       *  Left out entirely means the command must never be invoked. */
      capture?: (projectId: string) => unknown;
    } = {},
  ) {
    invokeMock.mockImplementation(async (cmd: string, args: unknown) => {
      const a = (args ?? {}) as { id?: string; projectId?: string };
      if (cmd === "list_projects") return projects;
      if (cmd === "project_status")
        return extra.status
          ? extra.status(a.id ?? "")
          : {
              state: "stopped",
              url: null,
              message: "Not running.",
              ready: false,
            };
      if (cmd === "pocket_thumb") return thumbFor(a.projectId ?? "");
      if (cmd === "pocket_thumb_capture") {
        if (extra.capture === undefined)
          throw new Error("pocket_thumb_capture was not expected");
        return extra.capture(a.projectId ?? "");
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });
  }

  const captureCalls = () =>
    invokeMock.mock.calls.filter((c) => c[0] === "pocket_thumb_capture");

  it("asks native for each listed project's thumb by id", async () => {
    mockProjectsAndThumbs([DRAFT, INSTALLED], () => null);
    render(<CreationPanel runtimeReady={true} page="live" />);
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("pocket_thumb", {
        projectId: "p-1",
      });
      expect(invokeMock).toHaveBeenCalledWith("pocket_thumb", {
        projectId: "p-2",
      });
    });
  });

  it("shows the captured UI with its honest as-of caption", async () => {
    mockProjectsAndThumbs([INSTALLED], () => THUMB);
    const { container } = render(
      <CreationPanel runtimeReady={true} page="live" />,
    );
    await screen.findByText("Timer Pocket");

    await waitFor(() => {
      expect(container.querySelector(".creation-thumb-image")).toBeTruthy();
    });
    const image = container.querySelector("img.creation-thumb-image")!;
    expect(image.getAttribute("src")).toBe(THUMB.dataUrl);
    // The image is decoration; the caption is the claim, so the alt stays
    // empty and the caption is exactly the stored mtime, phrased as a past
    // moment — never "current", never "live".
    expect(image.getAttribute("alt")).toBe("");
    const caption = container.querySelector(".creation-thumb-caption");
    expect(caption?.textContent).toBe(formatCapturedAt(CAPTURED_AT));
    // The exact moment stays one hover away.
    expect(caption?.getAttribute("title")).toBe(
      formatCapturedFull(CAPTURED_AT),
    );
  });

  it("enlarges the stored capture from the card and closes by button, backdrop and Escape", async () => {
    mockProjectsAndThumbs([INSTALLED], () => THUMB);
    const { container } = render(
      <CreationPanel runtimeReady={true} page="live" />,
    );
    await screen.findByText("Timer Pocket");
    await waitFor(() => {
      expect(container.querySelector(".creation-thumb-image")).toBeTruthy();
    });

    // The card's capture is a real button with a specific, honest name:
    // it announces the enlargement of *that* Pocket's captured view.
    const cardButton = container.querySelector<HTMLButtonElement>(
      "button.creation-thumb-button",
    )!;
    expect(cardButton.getAttribute("aria-label")).toContain(
      "Enlarge the captured view of Timer Pocket",
    );
    fireEvent.click(cardButton);

    const overlay = container.querySelector<HTMLElement>(
      '[role="dialog"][aria-modal="true"]',
    )!;
    expect(overlay.getAttribute("aria-label")).toContain(
      "Enlarged capture of Timer Pocket",
    );
    const enlargedImage = overlay.querySelector("img")!;
    // The same stored bytes, not a refetch: nothing new is asked of native
    // when opening the overlay, so it can only show what the card showed.
    expect(enlargedImage.getAttribute("src")).toBe(THUMB.dataUrl);
    expect(enlargedImage.getAttribute("alt")).toContain("Timer Pocket");
    expect(overlay.textContent).toContain(
      `captured ${formatCapturedFull(CAPTURED_AT)}`,
    );

    // Close by its own button...
    fireEvent.click(
      overlay.querySelector<HTMLButtonElement>(
        "button.creation-lightbox-close",
      )!,
    );
    expect(
      container.querySelector('[role="dialog"][aria-modal="true"]'),
    ).toBeNull();

    // ...by the backdrop (which reopens nothing)...
    fireEvent.click(container.querySelector("button.creation-thumb-button")!);
    expect(
      container.querySelector('[role="dialog"][aria-modal="true"]'),
    ).toBeTruthy();
    fireEvent.click(container.querySelector(".creation-modal-backdrop")!);
    expect(
      container.querySelector('[role="dialog"][aria-modal="true"]'),
    ).toBeNull();

    // ...and by Escape, the delete confirmation's idiom.
    fireEvent.click(container.querySelector("button.creation-thumb-button")!);
    fireEvent.keyDown(window, { key: "Escape" });
    expect(
      container.querySelector('[role="dialog"][aria-modal="true"]'),
    ).toBeNull();
  });

  it("keeps dating a frozen capture while a newer one lands behind the overlay", async () => {
    // First read: an older capture. The card click freezes it for the
    // overlay. A refreshKey bump refetches the thumbs behind the open
    // overlay (second read returns a newer capture) — the enlarged
    // picture and its date must not silently swap under the reader.
    let firstRead = true;
    mockProjectsAndThumbs([INSTALLED], () => {
      if (firstRead) {
        firstRead = false;
        return THUMB;
      }
      return {
        dataUrl: THUMB.dataUrl + "-newer",
        capturedAt: CAPTURED_AT + 60_000,
      };
    });
    const { container, rerender } = render(
      <CreationPanel runtimeReady={true} page="live" refreshKey={0} />,
    );
    await screen.findByText("Timer Pocket");
    await waitFor(() => {
      expect(container.querySelector(".creation-thumb-image")).toBeTruthy();
    });

    fireEvent.click(
      container.querySelector<HTMLButtonElement>(
        "button.creation-thumb-button",
      )!,
    );
    const overlay = container.querySelector(
      '[role="dialog"][aria-modal="true"]',
    )!;

    rerender(<CreationPanel runtimeReady={true} page="live" refreshKey={1} />);
    await waitFor(() => {
      // The card behind the overlay shows the newer capture now...
      expect(
        container
          .querySelector("img.creation-thumb-image")
          ?.getAttribute("src"),
      ).toBe(THUMB.dataUrl + "-newer");
    });
    // ...while the open overlay still shows — and dates — the capture it
    // was opened with.
    expect(overlay.querySelector("img")?.getAttribute("src")).toBe(
      THUMB.dataUrl,
    );
    expect(overlay.textContent).toContain(
      `captured ${formatCapturedFull(CAPTURED_AT)}`,
    );
  });

  it("renders no enlarge button on cards without a thumb", async () => {
    mockProjectsAndThumbs([INSTALLED], () => null);
    const { container } = render(
      <CreationPanel runtimeReady={true} page="live" />,
    );
    await screen.findByText("Timer Pocket");
    expect(container.querySelector("button.creation-thumb-button")).toBeNull();
  });

  it("renders no figure for absent, failed or malformed thumbs", async () => {
    // One quiet card is the only honest fallback for every non-answer.
    const noAnswer = [
      null,
      undefined,
      new Error("native read failed"),
      { dataUrl: "https://example.com/shot.png", capturedAt: CAPTURED_AT },
      { dataUrl: "javascript:alert(1)", capturedAt: CAPTURED_AT },
      { dataUrl: THUMB.dataUrl },
      { dataUrl: THUMB.dataUrl, capturedAt: Number.NaN },
    ];
    for (const answer of noAnswer) {
      cleanup();
      mockProjectsAndThumbs([INSTALLED], () => {
        if (answer instanceof Error) throw answer;
        return answer;
      });
      const { container } = render(
        <CreationPanel runtimeReady={true} page="live" />,
      );
      await screen.findByText("Timer Pocket");
      // The list itself always renders; only the thumb figure is decided.
      await waitFor(() => {
        expect(invokeMock).toHaveBeenCalledWith("pocket_thumb", {
          projectId: "p-2",
        });
      });
      expect(container.querySelector(".creation-thumb")).toBeNull();
      expect(container.querySelector(".creation-thumb-caption")).toBeNull();
    }
  });

  it("keeps good thumbs when another project's read fails", async () => {
    // A per-project failure is exactly that: per-project. The batch must
    // not blank the project whose read answered.
    const other = {
      ...INSTALLED,
      id: "p-3",
      manifest: {
        ...INSTALLED.manifest,
        metadata: {
          ...INSTALLED.manifest.metadata,
          id: "p-3",
          name: "Recipe Pocket",
        },
      },
    };
    mockProjectsAndThumbs([INSTALLED, other], (projectId) => {
      if (projectId === "p-3") throw new Error("p-3 read failed");
      return THUMB;
    });
    const { container } = render(
      <CreationPanel runtimeReady={true} page="live" />,
    );
    await screen.findByText("Recipe Pocket");

    await waitFor(() => {
      expect(container.querySelector(".creation-thumb-image")).toBeTruthy();
    });
    const cards = Array.from(container.querySelectorAll(".creation-card"));
    const goodCard = cards.find((card) =>
      card.textContent?.includes("Timer Pocket"),
    )!;
    const failedCard = cards.find((card) =>
      card.textContent?.includes("Recipe Pocket"),
    )!;
    expect(goodCard.querySelector(".creation-thumb")).toBeTruthy();
    expect(failedCard.querySelector(".creation-thumb")).toBeNull();
    expect(container.querySelectorAll(".creation-thumb")).toHaveLength(1);
  });

  it("refetches thumbs with the catalog on a refreshKey bump", async () => {
    mockProjectsAndThumbs([INSTALLED], () => null);
    const { rerender } = render(
      <CreationPanel runtimeReady={true} page="live" refreshKey={0} />,
    );
    await screen.findByText("Timer Pocket");
    await waitFor(() => {
      expect(
        invokeMock.mock.calls.filter((c) => c[0] === "pocket_thumb"),
      ).toHaveLength(1);
    });

    rerender(<CreationPanel runtimeReady={true} page="live" refreshKey={1} />);
    await waitFor(() => {
      expect(
        invokeMock.mock.calls.filter((c) => c[0] === "pocket_thumb"),
      ).toHaveLength(2);
    });
  });

  it("captures once for a running Pocket whose thumb read came back empty", async () => {
    // The live page read the Pocket running-and-ready and the stored read
    // said there is nothing — the UI behind the mapped port right now is
    // an honest capture moment, so the panel asks native for it.
    const capture = vi.fn(async () => THUMB);
    mockProjectsAndThumbs([INSTALLED], () => null, {
      status: () => RUNNING,
      capture,
    });
    const { container } = render(
      <CreationPanel runtimeReady={true} page="live" />,
    );

    // The capture's answer lands as a normal thumb figure, caption and all.
    await waitFor(() => {
      expect(container.querySelector(".creation-thumb-image")).toBeTruthy();
    });
    expect(capture).toHaveBeenCalledWith("p-2");
    const image = container.querySelector("img.creation-thumb-image")!;
    expect(image.getAttribute("src")).toBe(THUMB.dataUrl);
    expect(
      container.querySelector(".creation-thumb-caption")?.textContent,
    ).toBe(formatCapturedAt(CAPTURED_AT));
    // Exactly one ask per project per mount — the settled card must not
    // start a capture loop through its own state updates.
    expect(captureCalls()).toHaveLength(1);
    expect(capture).toHaveBeenCalledTimes(1);
  });

  it("asks for no capture when a thumb or the truth already says enough", async () => {
    // Held thumb: nothing to gain.
    const capture = vi.fn();
    mockProjectsAndThumbs([INSTALLED], () => THUMB, {
      status: () => RUNNING,
      capture,
    });
    const { container } = render(
      <CreationPanel runtimeReady={true} page="live" />,
    );
    await waitFor(() => {
      expect(container.querySelector(".creation-thumb-image")).toBeTruthy();
    });
    expect(capture).not.toHaveBeenCalled();

    // Stopped Pocket with no thumb: no rendered UI to claim, so no ask —
    // even though the thumb read itself answered null.
    cleanup();
    const stoppedCapture = vi.fn();
    mockProjectsAndThumbs([INSTALLED], () => null, {
      capture: stoppedCapture,
    });
    const second = render(<CreationPanel runtimeReady={true} page="live" />);
    await second.findByText("Timer Pocket");
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("pocket_thumb", {
        projectId: "p-2",
      });
    });
    expect(stoppedCapture).not.toHaveBeenCalled();
    expect(second.container.querySelector(".creation-thumb")).toBeNull();
  });

  it("leaves the plain card after a failed capture and never re-asks", async () => {
    // A refusal — busy runtime, non-macOS build, anything — is the same
    // quiet outcome as no capture moment: no figure, no error surfaced,
    // and the one-per-mount attempt budget stays spent across refreshes.
    const capture = vi.fn(async () => {
      throw new Error("another capture holds the claim");
    });
    mockProjectsAndThumbs([INSTALLED], () => null, {
      status: () => RUNNING,
      capture,
    });
    const { container, rerender } = render(
      <CreationPanel runtimeReady={true} page="live" refreshKey={0} />,
    );
    await waitFor(() => expect(capture).toHaveBeenCalledTimes(1));
    expect(container.querySelector(".creation-thumb")).toBeNull();

    rerender(<CreationPanel runtimeReady={true} page="live" refreshKey={1} />);
    await waitFor(() => {
      expect(
        invokeMock.mock.calls.filter((c) => c[0] === "pocket_thumb"),
      ).toHaveLength(2);
    });
    expect(capture).toHaveBeenCalledTimes(1);
    expect(container.querySelector(".creation-thumb")).toBeNull();
  });

  it("never asks native for captures from the drafts page", async () => {
    // A draft's UI exists only inside a live preview session; the page
    // guard means even an installed record viewed off-page (not rendered
    // there at all) triggers no capture from the drafts route.
    const capture = vi.fn();
    mockProjectsAndThumbs([INSTALLED], () => null, {
      status: () => RUNNING,
      capture,
    });
    render(<CreationPanel runtimeReady={true} page="drafts" />);
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("pocket_thumb", {
        projectId: "p-2",
      });
    });
    expect(capture).not.toHaveBeenCalled();
  });

  it("replaces a stale card thumbnail when native pushes pocket-thumb-updated", async () => {
    const win = window as unknown as Record<string, unknown>;
    const hadInternals = "__TAURI_INTERNALS__" in win;
    const prevInternals = win.__TAURI_INTERNALS__;
    win.__TAURI_INTERNALS__ = {};
    let handler: ((event: { payload: unknown }) => void) | null = null;
    const unlisten = vi.fn();
    listenMock.mockImplementation(async (eventName, cb) => {
      if (eventName === "pocket-thumb-updated") {
        handler = cb as (event: { payload: unknown }) => void;
      }
      return unlisten;
    });

    try {
      mockProjectsAndThumbs([INSTALLED], () => THUMB, {
        status: () => RUNNING,
      });
      const { unmount } = render(
        <CreationPanel runtimeReady={true} page="live" />,
      );
      const card = await screen.findByRole("article", { name: "Timer Pocket" });
      await waitFor(() => {
        expect(
          card.querySelector("img.creation-thumb-image")?.getAttribute("src"),
        ).toBe(THUMB.dataUrl);
      });
      await waitFor(() => expect(handler).not.toBeNull());

      const fresher = {
        dataUrl: "data:image/png;base64,bmV3LWNhcHR1cmU=",
        capturedAt: CAPTURED_AT + 60_000,
      };
      act(() => {
        handler?.({ payload: { projectId: INSTALLED.id, thumb: fresher } });
      });
      await waitFor(() => {
        expect(
          card.querySelector("img.creation-thumb-image")?.getAttribute("src"),
        ).toBe(fresher.dataUrl);
      });

      // An older late-arriving event never regresses the fresher capture.
      act(() => {
        handler?.({ payload: { projectId: INSTALLED.id, thumb: THUMB } });
      });
      expect(
        card.querySelector("img.creation-thumb-image")?.getAttribute("src"),
      ).toBe(fresher.dataUrl);

      unmount();
      expect(unlisten).toHaveBeenCalledTimes(1);
    } finally {
      listenMock.mockReset();
      if (hadInternals) win.__TAURI_INTERNALS__ = prevInternals;
      else delete win.__TAURI_INTERNALS__;
    }
  });
});

/**
 * CSS contract for the split management pages (same helper shape as the
 * sidebar's): the lineage caption is drawn from the existing label treatment
 * sidebar's): the lineage caption is drawn from the existing label treatment
 * and semantic tokens only, the grid's overflow floor is pinned against
 * regression, and everything the split retired stays retired.
 */
function topLevelRules(
  css: string,
): Array<{ selectors: string[]; body: string }> {
  const stripped = css
    .replace(/\/\*[\s\S]*?\*\//g, "")
    .replace(/@media[^{]+\{(?:[^{}]*\{[^{}]*\})*[^{}]*\}/g, "");
  const rules: Array<{ selectors: string[]; body: string }> = [];
  for (const match of stripped.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    rules.push({
      selectors: match[1]
        .split(",")
        .map((selector) => selector.trim())
        .filter((selector) => selector !== ""),
      body: match[2],
    });
  }
  return rules;
}

function soleRuleBody(css: string, selector: string): string {
  const bodies = topLevelRules(css)
    .filter((rule) => rule.selectors.includes(selector))
    .map((rule) => rule.body);
  expect(bodies).toHaveLength(1);
  return bodies[0];
}

describe("CreationPanel page layout contract", () => {
  it("draws the lineage caption as one dim line under the title", () => {
    // A fact about the card, styled like the card's dim metadata: the
    // description's own top offset, nothing louder.
    const caption = soleRuleBody(creationCss, ".creation-edited-from");
    expect(caption).toContain("margin: 0.25rem 0 0;");
    expect(caption).toContain("color: var(--text-dim);");

    // Nothing invented: no hex, no alpha color, no shadow, no radius, no
    // border, and every token an existing semantic color.
    expect(caption).not.toMatch(/#[0-9a-f]{3,8}/i);
    expect(caption).not.toMatch(/rgba?\(/);
    expect(caption).not.toContain("box-shadow");
    expect(caption).not.toContain("border-radius");
    expect(caption).not.toContain("border");
    for (const token of caption.matchAll(/var\((--[\w-]+)\)/g)) {
      expect(token[1]).toMatch(/^--(?:text|text-dim|accent)$/);
    }
  });

  /* Both declarations below exist for one bug and one bug only. The optional
     Technical details block renders the Containerfile in a `white-space: pre`
     `<pre>`, whose min-content width is its longest line, and a scroll
     container still contributes that min-content width to whatever measures
     it. So the expanded card grew past its grid track and painted over the
     neighbouring card. The content floor has to come off at both levels the
     `pre` is measured through — the card as the flex item of its `<li>` cell,
     and the review block's own implicit grid column — and a live browser repro
     confirmed both are needed: with only one of them the pre still escapes.
     Neither line is tidiness, and neither may be "simplified" away; the pre's
     own `overflow: auto` is what makes them safe. */
  it("keeps a Containerfile pre inside the card and inside its grid track", () => {
    // Level one: the card is the cell's flex item, and the default `auto`
    // minimum would let the pre widen it past the cell.
    const card = soleRuleBody(creationCss, ".creation-card");
    expect(card).toContain("min-width: 0;");

    // Level two: without an explicit track, the review grid's implicit
    // `auto` column takes the same content-based minimum from the pre.
    const review = soleRuleBody(creationCss, ".creation-review");
    expect(review).toContain("grid-template-columns: minmax(0, 1fr);");

    // And the thing that started it stays a scroll container rather than
    // being rewrapped to make the overflow go away.
    expect(soleRuleBody(creationCss, ".creation-containerfile")).toContain(
      "overflow: auto;",
    );
  });

  it("pins the thumb image to the card width with the whole capture visible", () => {
    // Native captures are a fixed 1280×720 desktop box now, but older
    // stored thumbs may carry another size, so the image stays a fluid
    // 16:9 box:
    // `width: 100%` keeps it inside the card — the same min-content lesson
    // as the Containerfile pin above, one element earlier — and `contain`
    // shows the captured UI whole instead of cropping away evidence.
    const image = soleRuleBody(creationCss, ".creation-thumb-image");
    expect(image).toContain("width: 100%;");
    expect(image).toContain("object-fit: contain;");
    expect(image).toContain("aspect-ratio: 16 / 9;");
    // Calm metadata caption, same tokens as the lineage caption.
    const caption = soleRuleBody(creationCss, ".creation-thumb-caption");
    expect(caption).toContain("color: var(--text-dim);");
  });

  it("keeps the enlarge affordance a flush button and the overlay image bounded", () => {
    // The card's image wrapper is a reset button, not a decorated control:
    // flush box (padding/border 0) so the image keeps its exact card
    // geometry, and the cursor says "bigger" rather than "press".
    const button = soleRuleBody(creationCss, "button.creation-thumb-button");
    expect(button).toContain("padding: 0;");
    expect(button).toContain("border: 0;");
    expect(button).toContain("cursor: zoom-in;");

    // The overlay frames any stored capture: `overflow: hidden` plus
    // `width: 100%` letterboxes legacy odd-sized thumbs inside the frame
    // instead of letting them stretch it, and `contain` keeps showing the
    // captured UI whole at full size — the card's promise, one level up.
    const frame = soleRuleBody(creationCss, ".creation-lightbox");
    expect(frame).toContain("overflow: hidden;");
    const image = soleRuleBody(creationCss, ".creation-lightbox-image");
    expect(image).toContain("width: 100%;");
    expect(image).toContain("object-fit: contain;");
  });

  it("retires the single-page section, tab, pile and group rules completely", () => {
    // Two routed pages need no section heading inside a page, no phase tabs
    // to switch, and none of the superseded lineage machinery. They must not
    // survive anywhere in the stylesheet — a raw sweep catches occurrences a
    // parsed top-level-rule pass would miss, such as a stray selector inside
    // another rule's list or inside a media query.
    for (const dead of [
      ".creation-section-title",
      ".creation-tabs",
      ".creation-tab",
      ".creation-pile",
      ".creation-group",
    ]) {
      expect(creationCss).not.toContain(dead);
    }
    for (const rule of topLevelRules(creationCss)) {
      for (const selector of rule.selectors) {
        expect(selector).not.toMatch(
          /creation-(?:section-title|tabs?|pile|group)/,
        );
      }
    }
  });
});

describe("formatCapturedAt", () => {
  const now = new Date(2026, 10, 21, 17, 30, 0).getTime();
  const ago = (ms: number) => formatCapturedAt(now - ms, now);
  const MIN = 60_000;
  const HOUR = 60 * MIN;

  it("speaks in the units a glance needs", () => {
    expect(ago(20_000)).toBe("Captured just now");
    expect(ago(5 * MIN)).toBe("Captured 5 min ago");
    expect(ago(3 * HOUR)).toBe("Captured 3h ago");
    expect(ago(30 * HOUR)).toBe("Captured yesterday");
  });

  it("falls back to a short date, with the year only when it differs", () => {
    const sameYear = new Date(2026, 10, 2, 9, 0).getTime();
    expect(formatCapturedAt(sameYear, now)).toBe(
      `Captured ${new Date(sameYear).toLocaleDateString(undefined, { day: "numeric", month: "short" })}`,
    );
    const lastYear = new Date(2025, 4, 9, 9, 0).getTime();
    expect(formatCapturedAt(lastYear, now)).toContain("2025");
  });

  it("never shows seconds and treats a future stamp as just now", () => {
    expect(formatCapturedAt(now + 5 * MIN, now)).toBe("Captured just now");
    expect(formatCapturedFull(now)).not.toMatch(/:\d{2}:\d{2}/);
  });

  it("formats the built timestamp with local date and short time", () => {
    const seconds = Math.floor(now / 1000);
    expect(formatBuiltDate(seconds)).toBe(
      new Date(seconds * 1000).toLocaleString(undefined, {
        dateStyle: "short",
        timeStyle: "short",
      }),
    );
    expect(formatBuiltDate(seconds)).toMatch(/\d:\d{2}/);
  });
});

describe("pocket card layout", () => {
  it("keeps the address row inside the card", () => {
    // A content-sized track would let address + copy + Tailscale badge push
    // past the card; the row wraps the badge instead.
    expect(soleRuleBody(creationCss, ".creation-lifecycle")).toContain(
      "grid-template-columns: minmax(0, 1fr);",
    );
    expect(soleRuleBody(creationCss, ".creation-address")).toContain(
      "flex-wrap: wrap;",
    );
  });

  it("keeps the Manage sections inside the card", () => {
    // An auto grid column grows to the widest unwrapped line (a long
    // Versions summary) and pushes every section past the card's edge.
    expect(soleRuleBody(creationCss, ".creation-card-disclosures")).toContain(
      "grid-template-columns: minmax(0, 1fr);",
    );
  });

  it("keeps the action row on one line", () => {
    expect(soleRuleBody(creationCss, ".creation-card-buttons")).toContain(
      "flex-wrap: nowrap;",
    );
  });

  it("keeps the description and address in mono", () => {
    // Reading copy (hints, notes) switches to the system sans in styles.css;
    // the card's description and its address are identity and data.
    expect(soleRuleBody(creationCss, ".creation-description")).not.toContain(
      "--font-sans",
    );
    expect(soleRuleBody(creationCss, ".creation-address")).toContain(
      "font-family: var(--font-mono);",
    );
  });

  it("shows addresses without the scheme or a trailing slash", () => {
    expect(displayAddress("http://pocket-p2.localhost:18789")).toBe(
      "pocket-p2.localhost:18789",
    );
    expect(displayAddress("https://pocket-p2.localhost:18789/")).toBe(
      "pocket-p2.localhost:18789",
    );
  });
});
