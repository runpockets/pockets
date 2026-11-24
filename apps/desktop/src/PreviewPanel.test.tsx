import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ComponentProps } from "react";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  within,
} from "@testing-library/react";
import PreviewPanelView from "./PreviewPanel";
import PreviewNetworkSettings from "./PreviewNetworkSettings";
import { useProjectPreview } from "./useProjectPreview";
import { invoke } from "./api";
import previewCss from "./PreviewPanel.css?raw";
import stylesCss from "./styles.css?raw";

// The panel only ever talks to the native backend through `invoke`; the real
// module would fail outside Tauri, so mock the whole module.
vi.mock("./api", () => ({ invoke: vi.fn() }));

const invokeMock = vi.mocked(invoke);

function PreviewPanel({
  projectId,
  showAccessSettings = false,
  ...props
}: Omit<ComponentProps<typeof PreviewPanelView>, "preview"> & {
  projectId: string | null;
  showAccessSettings?: boolean;
}) {
  const preview = useProjectPreview(projectId);
  return (
    <>
      {showAccessSettings && projectId && (
        <PreviewNetworkSettings
          projectId={projectId}
          access={preview.apiAccess}
        />
      )}
      <PreviewPanelView preview={preview} {...props} />
    </>
  );
}

/** Session id in the shape native `validate_session_id` accepts: exactly 32
 *  lowercase hex characters. */
const SESSION = "3f2a9c8e51b64d7fa0c1d2e3f4a5b6c7";

/** Status payload shaped like the native `PreviewStatus` view: `url` and
 *  `error` keys are serde-optional, and the fixture omits them by default
 *  exactly like native does. */
function statusFixture(overrides: Record<string, unknown> = {}) {
  return {
    state: "stopped",
    revision: 0,
    eventsSince: [] as unknown[],
    nextCursor: 0,
    ...overrides,
  };
}

const krakenReview = {
  token: "kraken-review",
  services: [
    {
      host: "api.kraken.com",
      name: "Kraken pricing",
      purpose: "Show live BTC prices.",
    },
  ],
};

/**
 * Minimal ResizeObserver stand-in: jsdom ships none. `observe` records the
 * target, `deliver` replays the resize callback the real observer fires when
 * the observed box changes. The panel also reports once on mount itself, so
 * the constructor does not auto-fire.
 */
class ResizeObserverStub {
  static instances: ResizeObserverStub[] = [];
  callback: ResizeObserverCallback;
  targets: Element[] = [];
  constructor(callback: ResizeObserverCallback) {
    this.callback = callback;
    ResizeObserverStub.instances.push(this);
  }
  observe(target: Element) {
    this.targets.push(target);
  }
  disconnect() {
    this.targets = [];
  }
  deliver() {
    this.callback(
      this.targets.map((target) => ({ target }) as ResizeObserverEntry),
      this as unknown as ResizeObserver,
    );
  }
}

/**
 * requestAnimationFrame stand-in for the v3 §6.5 coalescing: jsdom's real
 * frames run on timers nothing here drives, so callbacks queue until
 * `flushFrames()` runs them — one flush is exactly one frame. A test proves
 * "several signals, one frame" by raising them all before the single flush;
 * unmount-time `cancelAnimationFrame` drops a queued frame before it runs.
 * Armed by `beforeEach` for every panel mount, dropped with the other
 * globals in `afterEach`.
 */
let frameQueue: Array<{ id: number; run: () => void }> = [];
let frameSeq = 0;

function flushFrames() {
  const pending = frameQueue;
  frameQueue = [];
  for (const frame of pending) {
    act(() => {
      frame.run();
    });
  }
}

const visibilityDescriptor = Object.getOwnPropertyDescriptor(
  Object.getPrototypeOf(document) as object,
  "visibilityState",
);

/** Flips `document.visibilityState` and announces the change, as in the
 *  ResourceMonitor sibling — the panel pauses polling while hidden. */
function setVisibility(value: "visible" | "hidden") {
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    get: () => value,
  });
  act(() => {
    document.dispatchEvent(new Event("visibilitychange"));
  });
}

function statusCalls(): unknown[] {
  return invokeMock.mock.calls
    .filter((call) => call[0] === "project_preview_status")
    .map((call) => call[1]);
}

/** Flushes the immediate first status read (a promise, no timers involved). */
async function settleFirstRead() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(0);
  });
}

/** Advances one poll tick (700 ms, AiPanel's cadence). */
async function tickOnce() {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(700);
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  ResizeObserverStub.instances = [];
  vi.stubGlobal("ResizeObserver", ResizeObserverStub);
  frameQueue = [];
  frameSeq = 0;
  vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    frameSeq += 1;
    const id = frameSeq;
    frameQueue.push({ id, run: () => callback(0) });
    return id;
  });
  vi.stubGlobal("cancelAnimationFrame", (id: number) => {
    frameQueue = frameQueue.filter((frame) => frame.id !== id);
  });
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  if (visibilityDescriptor) {
    Object.defineProperty(document, "visibilityState", visibilityDescriptor);
  }
});

describe("PreviewPanel without a session", () => {
  it.each([
    ["Allow", true],
    ["Ignore", false],
  ])(
    "starts before the user selects %s for a declared service",
    async (button, allow) => {
      vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
      const live = {
        state: "ready",
        revision: 2,
        url: "http://127.0.0.1:4567",
      };
      let started = false;
      let accessSaved = false;
      invokeMock.mockImplementation(async (command) => {
        if (command === "project_preview_status") {
          return started
            ? statusFixture({ ...live, apiAccess: krakenReview })
            : statusFixture();
        }
        if (command === "project_preview_start") {
          started = true;
          return statusFixture({
            state: "preparing",
            revision: 1,
            apiAccess: krakenReview,
          });
        }
        if (command === "project_preview_review_api_access") {
          accessSaved = allow;
          return statusFixture(live);
        }
        if (command === "project_preview_network")
          return {
            hosts: accessSaved ? ["api.kraken.com"] : [],
            previewApis: [],
          };
        throw new Error(`Unexpected command: ${command}`);
      });
      render(<PreviewPanel projectId={SESSION} showAccessSettings />);
      await settleFirstRead();
      await act(async () =>
        fireEvent.click(screen.getByRole("button", { name: "Start" })),
      );
      expect(invokeMock).toHaveBeenCalledWith("project_preview_start", {
        projectId: SESSION,
      });
      expect(screen.getByText("Preparing…")).toBeTruthy();
      expect(screen.getByText("Kraken pricing")).toBeTruthy();
      expect(screen.getByText("api.kraken.com")).toBeTruthy();
      expect(screen.getByText("Show live BTC prices.")).toBeTruthy();
      const summary = screen.getByText("Access requested").closest("summary");
      const access = summary?.closest("details");
      expect(access?.open).toBe(true);
      expect(access?.getAttribute("data-approval-needed")).toBe("true");
      expect(document.activeElement).toBe(summary);
      await tickOnce();
      expect(screen.getByText("Ready")).toBeTruthy();
      expect(screen.getByText("Access requested")).toBeTruthy();
      await act(async () =>
        fireEvent.click(screen.getByRole("button", { name: button })),
      );
      expect(invokeMock).toHaveBeenCalledWith(
        "project_preview_review_api_access",
        {
          projectId: SESSION,
          decision: { token: "kraken-review", allow },
        },
      );
      expect(
        invokeMock.mock.calls.filter(
          ([command]) => command === "project_preview_start",
        ),
      ).toHaveLength(1);
      expect(screen.queryByText("Kraken pricing")).toBeNull();
      expect(access?.hasAttribute("data-approval-needed")).toBe(false);
      const field = screen.getByLabelText<HTMLInputElement>("Add a hostname");
      expect(field.value).toBe("");
      expect(field.disabled).toBe(true);
      if (allow) {
        const list = within(
          screen.getByRole("list", { name: "Allowed hostnames" }),
        );
        expect(list.getByText("api.kraken.com")).toBeTruthy();
        expect(
          list.getByRole<HTMLButtonElement>("button", {
            name: "Remove api.kraken.com",
          }).disabled,
        ).toBe(true);
      } else {
        expect(screen.getByText(/^No hostnames yet\./)).toBeTruthy();
      }
    },
  );

  it("reviews new access on a running preview without issuing Stop", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const live = { state: "ready", revision: 2, url: "http://127.0.0.1:4567" };
    const withReview = statusFixture({ ...live, apiAccess: krakenReview });
    let polls = 0;
    let resolveStalePoll!: (status: typeof withReview) => void;
    const stalePoll = new Promise<typeof withReview>((resolve) => {
      resolveStalePoll = resolve;
    });
    invokeMock.mockImplementation(async (command) => {
      if (command === "project_preview_status") {
        polls += 1;
        if (polls === 1) return withReview;
        if (polls === 2) return stalePoll;
        return statusFixture(live);
      }
      if (command === "project_preview_review_api_access")
        return statusFixture(live);
      if (command === "project_preview_network")
        return { hosts: ["api.kraken.com"], previewApis: [] };
      throw new Error(`Unexpected command: ${command}`);
    });
    render(<PreviewPanel projectId={SESSION} showAccessSettings />);
    await settleFirstRead();
    expect(
      within(
        screen.getByRole("region", { name: "Workspace preview" }),
      ).queryByText("Kraken pricing"),
    ).toBeNull();
    await tickOnce();
    await act(async () =>
      fireEvent.click(screen.getByRole("button", { name: "Allow" })),
    );
    expect(invokeMock).toHaveBeenCalledWith(
      "project_preview_review_api_access",
      {
        projectId: SESSION,
        decision: { token: "kraken-review", allow: true },
      },
    );
    await act(async () => resolveStalePoll(withReview));
    expect(screen.queryByText("Kraken pricing")).toBeNull();
    expect(screen.getByText("127.0.0.1:4567")).toBeTruthy();
    expect(
      invokeMock.mock.calls.some(
        ([command]) => command === "project_preview_stop",
      ),
    ).toBe(false);
  });

  it("uses project-owned controls without an AI session and keeps Stop explicit", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    let running = false;
    let revision = 0;
    invokeMock.mockImplementation(async (command) => {
      if (command === "project_preview_start") {
        running = true;
        revision = 1;
        return statusFixture({
          state: "ready",
          revision,
          url: "http://127.0.0.1:4567",
        });
      }
      if (command === "project_preview_stop") {
        running = false;
        revision = 2;
        return true;
      }
      if (command === "project_preview_open") return undefined;
      if (command === "project_preview_status")
        return statusFixture({
          state: running ? "ready" : "stopped",
          revision,
          url: running ? "http://127.0.0.1:4567" : undefined,
        });
      throw new Error(`Unexpected command: ${command}`);
    });
    render(<PreviewPanel projectId={SESSION} />);
    await settleFirstRead();
    expect(invokeMock).toHaveBeenCalledWith("project_preview_status", {
      projectId: SESSION,
      cursor: 0,
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Start" }));
    });
    expect(invokeMock).toHaveBeenCalledWith("project_preview_start", {
      projectId: SESSION,
    });
    expect(screen.getByText("Ready")).toBeTruthy();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Open in browser" }));
    });
    expect(invokeMock).toHaveBeenCalledWith("project_preview_open", {
      projectId: SESSION,
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    });
    await tickOnce();
    await tickOnce();
    expect(invokeMock).toHaveBeenCalledWith("project_preview_stop", {
      projectId: SESSION,
    });
    expect(
      invokeMock.mock.calls.filter(
        ([command]) => command === "project_preview_start",
      ),
    ).toHaveLength(1);
    expect(
      invokeMock.mock.calls.some(([command]) => command.startsWith("ai_")),
    ).toBe(false);
  });

  it("renders a calm placeholder and invokes nothing", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      throw new Error(`Unexpected command: ${cmd}`);
    });

    render(<PreviewPanel projectId={null} />);

    expect(await screen.findByText(/Link or export this Pocket/)).toBeTruthy();
    // A placeholder is guidance, not a wall of controls or errors: nothing
    // is asked of native until a session exists.
    expect(screen.queryByRole("button", { name: "Start" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Stop" })).toBeNull();
    expect(
      screen.queryByRole("button", { name: "Open in browser" }),
    ).toBeNull();
    expect(screen.queryByText(/failed/i)).toBeNull();
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("treats a malformed session id like no session at all", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      throw new Error(`Unexpected command: ${cmd}`);
    });

    // The native guard is exactly 32 lowercase hex chars; the panel mirrors
    // it rather than ever sending a malformed id to a command.
    render(<PreviewPanel projectId={"not-a-session-id"} />);

    expect(await screen.findByText(/Link or export this Pocket/)).toBeTruthy();
    expect(invokeMock).not.toHaveBeenCalled();
  });
});

describe("PreviewPanel status polling", () => {
  it("advances the revision cursor and renders each state", async () => {
    const polls = [
      statusFixture({
        state: "preparing",
        revision: 1,
        eventsSince: [{ revision: 1, state: "preparing", atSeconds: 100 }],
        nextCursor: 1,
      }),
      statusFixture({
        state: "updating",
        revision: 2,
        eventsSince: [{ revision: 2, state: "updating", atSeconds: 101 }],
        nextCursor: 2,
      }),
      statusFixture({
        state: "ready",
        revision: 3,
        url: "http://127.0.0.1:49312/",
        eventsSince: [{ revision: 3, state: "ready", atSeconds: 102 }],
        nextCursor: 3,
      }),
    ];
    let pollIndex = 0;
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "project_preview_status") {
        return polls[Math.min(pollIndex++, polls.length - 1)];
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });

    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const { container } = render(<PreviewPanel projectId={SESSION} />);

    // The first read fires on mount, not one interval late.
    await settleFirstRead();
    expect(screen.getByText("Preparing…")).toBeTruthy();
    expect(invokeMock).toHaveBeenCalledWith("project_preview_status", {
      projectId: SESSION,
      cursor: 0,
    });
    // Open-in-browser is inert until a URL exists.
    expect(
      (
        screen.getByRole("button", {
          name: "Open in browser",
        }) as HTMLButtonElement
      ).disabled,
    ).toBe(true);

    await tickOnce();
    expect(screen.getByText("Rebuilding…")).toBeTruthy();

    await tickOnce();
    expect(screen.getByText("Ready")).toBeTruthy();

    // The loopback proxy URL renders as escaped text only — never a link:
    // there is no native webview yet, and the frontend never navigates to
    // or anchors a status URL (same rule as every other panel here).
    expect(screen.getByText("127.0.0.1:49312")).toBeTruthy();
    expect(container.querySelector("a")).toBeNull();

    // Cursor-based incremental reads, advancing by revision in order.
    expect(statusCalls()).toEqual([
      { projectId: SESSION, cursor: 0 },
      { projectId: SESSION, cursor: 1 },
      { projectId: SESSION, cursor: 2 },
    ]);

    // Ready is not terminal: the next chat turn must be observed as it
    // transitions through updating, so polling keeps running.
    await tickOnce();
    expect(statusCalls()).toHaveLength(4);
    expect(statusCalls()[3]).toEqual({ projectId: SESSION, cursor: 3 });
  });

  it("shows the failed state's message — event ring first, then error", async () => {
    const polls = [
      statusFixture({ state: "preparing", revision: 1, nextCursor: 1 }),
      statusFixture({
        state: "failed",
        revision: 2,
        // `error` omitted exactly like serde does; the message lives on the
        // transition event alone.
        eventsSince: [
          {
            revision: 2,
            state: "failed",
            atSeconds: 111,
            message:
              "Container restart hit MODULE_NOT_FOUND — the rename never propagated.",
          },
        ],
        nextCursor: 2,
      }),
      statusFixture({
        state: "failed",
        revision: 3,
        error: "The preview proxy never answered.",
        eventsSince: [{ revision: 3, state: "failed", atSeconds: 112 }],
        nextCursor: 3,
      }),
    ];
    let pollIndex = 0;
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "project_preview_status") {
        return polls[Math.min(pollIndex++, polls.length - 1)];
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });

    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    render(<PreviewPanel projectId={SESSION} />);
    await settleFirstRead();

    await tickOnce();
    expect(screen.getByText("Preview error")).toBeTruthy();
    expect(screen.getByText(/MODULE_NOT_FOUND/)).toBeTruthy();

    // The status error field wins over the older event message, and the
    // stale message does not linger.
    await tickOnce();
    expect(screen.getByText("Preview error")).toBeTruthy();
    // The detail renders inside `The preview failed. <message>`, so match the
    // substring rather than a string the component never produces.
    expect(screen.getByText(/preview proxy never answered/)).toBeTruthy();
    expect(screen.queryByText(/MODULE_NOT_FOUND/)).toBeNull();
  });

  it("keeps the last known state marked stale through a failed poll, and stops polling when hidden or unmounted", async () => {
    let failing = false;
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "project_preview_status") {
        if (failing) throw new Error("command not available");
        return statusFixture({
          state: "ready",
          revision: 1,
          url: "http://127.0.0.1:49312/",
          nextCursor: 1,
        });
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });

    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const { unmount } = render(<PreviewPanel projectId={SESSION} />);
    await settleFirstRead();
    expect(screen.getByText("Ready")).toBeTruthy();
    expect(statusCalls()).toHaveLength(1);

    // A status-read failure is not a preview failure: the state stays,
    // clearly marked as the last known one, and polling keeps retrying.
    failing = true;
    await tickOnce();
    expect(screen.getByText("Ready")).toBeTruthy();
    expect(
      screen.getByText(/Could not refresh the preview status/),
    ).toBeTruthy();
    failing = false;
    await tickOnce();
    expect(screen.queryByText(/Could not refresh/)).toBeNull();

    // Hidden: the loop pauses entirely — no reads fire — and returning to
    // the foreground resumes with a fresh sample right away.
    const seen = statusCalls().length;
    setVisibility("hidden");
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2100);
    });
    expect(statusCalls()).toHaveLength(seen);
    setVisibility("visible");
    await settleFirstRead();
    expect(statusCalls()).toHaveLength(seen + 1);

    // Unmount: no timer survives the panel.
    const beforeUnmount = statusCalls().length;
    unmount();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });
    expect(statusCalls()).toHaveLength(beforeUnmount);
  });
});

describe("PreviewPanel controls", () => {
  it("Start sends only the session id and adopts the returned status", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "project_preview_status") {
        return statusFixture({
          state: "stopped",
          revision: 1,
          eventsSince: [{ revision: 1, state: "stopped", atSeconds: 1 }],
          nextCursor: 1,
        });
      }
      if (cmd === "project_preview_start") {
        return statusFixture({
          state: "preparing",
          revision: 2,
          eventsSince: [{ revision: 2, state: "preparing", atSeconds: 2 }],
          nextCursor: 2,
        });
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });

    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    render(<PreviewPanel projectId={SESSION} />);
    await settleFirstRead();

    // A stopped preview is one click from running: Start is the primary
    // control and Stop is not offered.
    expect(screen.getByText("Stopped")).toBeTruthy();
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Start" }));
    });

    // Exact argument shape: only the session id crosses, never a path,
    // port, or URL.
    expect(invokeMock).toHaveBeenCalledWith("project_preview_start", {
      projectId: SESSION,
    });
    expect(screen.getByText("Preparing…")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Start" })).toBeNull();
    expect(screen.getByRole("button", { name: "Stop" })).toBeTruthy();
  });

  it("Stop sends only the session id and defers the state to the next poll", async () => {
    const polls = [
      statusFixture({
        state: "ready",
        revision: 1,
        url: "http://127.0.0.1:49312/",
        nextCursor: 1,
      }),
      statusFixture({ state: "stopped", revision: 2, nextCursor: 2 }),
    ];
    let pollIndex = 0;
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "project_preview_status") {
        return polls[Math.min(pollIndex++, polls.length - 1)];
      }
      if (cmd === "project_preview_stop") return true;
      throw new Error(`Unexpected command: ${cmd}`);
    });

    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    render(<PreviewPanel projectId={SESSION} />);
    await settleFirstRead();
    expect(screen.getByText("Ready")).toBeTruthy();

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    });
    expect(invokeMock).toHaveBeenCalledWith("project_preview_stop", {
      projectId: SESSION,
    });
    // Stop resolves with a bare bool: the panel never claims a stop native
    // has not confirmed, so the displayed state stays as reported until the
    // poll catches up.
    expect(screen.getByText("Ready")).toBeTruthy();

    await tickOnce();
    expect(screen.getByText("Stopped")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Start" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Start preview" })).toBeTruthy();
  });

  it("collapses the status lane when ready and offers a Copy preview URL action", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      configurable: true,
      value: { writeText },
    });
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "project_preview_status") {
        return statusFixture({
          state: "ready",
          revision: 1,
          url: "http://127.0.0.1:49312/",
          nextCursor: 1,
        });
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });

    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const { container } = render(<PreviewPanel projectId={SESSION} />);
    await settleFirstRead();
    expect(container.querySelector(".preview-status")?.textContent).toBe("");
    fireEvent.click(screen.getByRole("button", { name: "Copy preview URL" }));
    expect(writeText).toHaveBeenCalledWith("http://127.0.0.1:49312/");
  });

  it("says so calmly when Stop finds nothing to stop", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "project_preview_status") {
        return statusFixture({
          state: "updating",
          revision: 1,
          nextCursor: 1,
        });
      }
      if (cmd === "project_preview_stop") return false;
      throw new Error(`Unexpected command: ${cmd}`);
    });

    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    render(<PreviewPanel projectId={SESSION} />);
    await settleFirstRead();

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Stop" }));
    });
    // Fake timers: findByText's internal waiting needs the sibling's flush
    // convention instead of wall-clock waiting.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(
      screen.getByText("There was no active preview to stop."),
    ).toBeTruthy();
    // An explanation, not a crash: no unhandled failure anywhere else.
    expect(screen.queryByText(/Stopping the preview failed/)).toBeNull();
  });

  it("surfaces an project_preview_open refusal as an inline notice, not an error", async () => {
    const polls = [
      statusFixture({ state: "preparing", revision: 1, nextCursor: 1 }),
      statusFixture({
        state: "ready",
        revision: 2,
        url: "http://127.0.0.1:49312/",
        nextCursor: 2,
      }),
    ];
    let pollIndex = 0;
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "project_preview_status") {
        return polls[Math.min(pollIndex++, polls.length - 1)];
      }
      if (cmd === "project_preview_open") {
        // A native refusal (e.g. the preview stopped between the poll and
        // the click): the reason is surfaced verbatim, calmly.
        throw new Error("There is no preview running for this session.");
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });

    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const { container } = render(<PreviewPanel projectId={SESSION} />);
    await settleFirstRead();

    await tickOnce();
    expect(screen.getByText("Ready")).toBeTruthy();

    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Open in browser" }));
    });
    expect(invokeMock).toHaveBeenCalledWith("project_preview_open", {
      projectId: SESSION,
    });

    // The native reason lands verbatim in the hint lane — never an error
    // block, never a link, never unhandled.
    const notice = screen.getByText(
      /could not open the preview\. there is no preview running for this session\./i,
    );
    expect(notice.className).toContain("hint");
    expect(container.querySelector(".error")).toBeNull();
    expect(container.querySelector("a")).toBeNull();
  });
});

describe("PreviewPanel bounds reporting", () => {
  it("reports the reserved rect on mount and on every resize", () => {
    // The rect is in CSS logical pixels — the unit T3's native
    // `project_preview_set_bounds` placement plumbing consumes.
    const rectSpy = vi
      .spyOn(Element.prototype, "getBoundingClientRect")
      .mockReturnValue(new DOMRect(12, 34, 480, 320));
    const onBounds = vi.fn();

    render(<PreviewPanel projectId={null} onBounds={onBounds} />);

    expect(onBounds).toHaveBeenCalledTimes(1);
    expect(onBounds).toHaveBeenLastCalledWith({
      x: 12,
      y: 34,
      width: 480,
      height: 320,
    });
    expect(ResizeObserverStub.instances).toHaveLength(1);
    expect(ResizeObserverStub.instances[0].targets).toHaveLength(1);
    // The observed element is the reserved viewport region, not the card:
    // the header controls and status lines above it stay outside the
    // native pane's reach.
    expect(
      ResizeObserverStub.instances[0].targets[0].classList.contains(
        "preview-viewport",
      ),
    ).toBe(true);

    // A window or rail resize re-reports the new rect so the native pane can
    // follow it (the spike measured that move at median 1 ms) — through the
    // coalesced frame now (v3 §6.5), not from inside the observer callback.
    rectSpy.mockReturnValue(new DOMRect(12, 34, 640, 400));
    ResizeObserverStub.instances[0].deliver();
    expect(onBounds).toHaveBeenCalledTimes(1); // nothing sent mid-frame
    flushFrames();
    expect(onBounds).toHaveBeenCalledTimes(2);
    expect(onBounds).toHaveBeenLastCalledWith({
      x: 12,
      y: 34,
      width: 640,
      height: 400,
    });
  });

  it("re-reports when a scroll moves the reserved rect, and drops its listeners on unmount", () => {
    // The shell scrolls inside panes only: a scroll in a nested scroller
    // moves the reserved rect without resizing it, which a
    // ResizeObserver cannot see — a pane following only resizes would
    // drift over the controls. Scroll events do not bubble, so only a
    // capture-phase window listener catches them from nested scrollers.
    const rectSpy = vi
      .spyOn(Element.prototype, "getBoundingClientRect")
      .mockReturnValue(new DOMRect(12, 34, 480, 320));
    const addSpy = vi.spyOn(window, "addEventListener");
    const removeSpy = vi.spyOn(window, "removeEventListener");
    const onBounds = vi.fn();

    const { unmount } = render(
      <PreviewPanel projectId={null} onBounds={onBounds} />,
    );
    expect(onBounds).toHaveBeenCalledTimes(1);

    const scrollArm = addSpy.mock.calls.find(([type]) => type === "scroll");
    expect(scrollArm).toBeDefined();
    expect(scrollArm?.[2]).toBe(true);

    // A pane scroll shifts the viewport up — the report must carry the
    // moved coordinates, negative y (scrolled past the top) included. The
    // coalesced frame carries it (v3 §6.5): raised by the event, shipped
    // by the flush.
    rectSpy.mockReturnValue(new DOMRect(12, -108, 480, 320));
    const scroller = document.createElement("div");
    document.body.appendChild(scroller);
    fireEvent.scroll(scroller);
    document.body.removeChild(scroller);
    flushFrames();
    expect(onBounds).toHaveBeenCalledTimes(2);
    expect(onBounds).toHaveBeenLastCalledWith({
      x: 12,
      y: -108,
      width: 480,
      height: 320,
    });

    // A window resize re-reports the new rect as well.
    rectSpy.mockReturnValue(new DOMRect(0, 20, 390, 260));
    fireEvent(window, new Event("resize"));
    flushFrames();
    expect(onBounds).toHaveBeenCalledTimes(3);
    expect(onBounds).toHaveBeenLastCalledWith({
      x: 0,
      y: 20,
      width: 390,
      height: 260,
    });

    // Unmount removes both listeners exactly as armed: a dead panel never
    // reports again.
    unmount();
    expect(removeSpy).toHaveBeenCalledWith("scroll", scrollArm?.[1], true);
    const resizeArm = addSpy.mock.calls.find(([type]) => type === "resize");
    expect(resizeArm).toBeDefined();
    expect(removeSpy).toHaveBeenCalledWith("resize", resizeArm?.[1]);
  });

  it("re-reports when the session arrives after mount and when Ready lands", async () => {
    // The bug this pins: the panel mounts with no session (App's placement
    // callback ignores sessionless reports), the user finishes a generation
    // and clicks Start — and nothing ever reported the rect again unless the
    // window was resized, so the native pane never appeared.
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "project_preview_status") {
        return statusFixture({ state: "ready", revision: 1, nextCursor: 1 });
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });
    const rectSpy = vi
      .spyOn(Element.prototype, "getBoundingClientRect")
      .mockReturnValue(new DOMRect(5, 6, 100, 200));
    const onBounds = vi.fn();

    const view = render(<PreviewPanel projectId={null} onBounds={onBounds} />);
    // Sessionless: only the mount report, which App will discard anyway.
    expect(onBounds).toHaveBeenCalledTimes(1);

    // The generation completes: App points the panel at the session. That
    // alone must re-report so a preview started elsewhere is placed.
    await view.rerender(
      <PreviewPanel projectId={SESSION} onBounds={onBounds} />,
    );
    expect(onBounds).toHaveBeenCalledTimes(2);

    // The first poll answers Ready: the pane's create gate lives natively,
    // so the Ready transition itself must push a fresh rect.
    await settleFirstRead();
    expect(screen.getByText("Ready")).toBeTruthy();
    expect(onBounds).toHaveBeenCalledTimes(3);
    expect(onBounds).toHaveBeenLastCalledWith({
      x: 5,
      y: 6,
      width: 100,
      height: 200,
    });

    // And steady-state polls (same state, same url) stay quiet: no report
    // spam behind the advisory placement channel.
    await tickOnce();
    expect(onBounds).toHaveBeenCalledTimes(3);
    rectSpy.mockRestore();
  });

  it("coalesces every signal of one frame into a single freshest report", () => {
    // v3 §6.5: a window or divider drag delivers observer and window-resize
    // signals back to back, and the rect moves between them. Only the last
    // state of the frame may reach `onBounds` — never an intermediate rect.
    const rectSpy = vi
      .spyOn(Element.prototype, "getBoundingClientRect")
      .mockReturnValue(new DOMRect(12, 34, 480, 320));
    const onBounds = vi.fn();

    render(<PreviewPanel projectId={null} onBounds={onBounds} />);
    expect(onBounds).toHaveBeenCalledTimes(1); // immediate mount report

    rectSpy.mockReturnValue(new DOMRect(12, 34, 500, 340));
    ResizeObserverStub.instances[0].deliver();
    rectSpy.mockReturnValue(new DOMRect(12, 34, 560, 380));
    fireEvent(window, new Event("resize"));
    ResizeObserverStub.instances[0].deliver();
    expect(onBounds).toHaveBeenCalledTimes(1); // still one pending frame

    flushFrames();
    expect(onBounds).toHaveBeenCalledTimes(2);
    expect(onBounds).toHaveBeenLastCalledWith({
      x: 12,
      y: 34,
      width: 560,
      height: 380,
    });
    rectSpy.mockRestore();
  });

  it("skips a coalesced send when the rounded rect has not moved", () => {
    // Sub-pixel jitter inside one rounding step is not a move: the pane is
    // already exactly there (v3 §6.5). A real move still gets through.
    const rectSpy = vi
      .spyOn(Element.prototype, "getBoundingClientRect")
      .mockReturnValue(new DOMRect(12, 34, 480, 320));
    const onBounds = vi.fn();

    render(<PreviewPanel projectId={null} onBounds={onBounds} />);
    expect(onBounds).toHaveBeenCalledTimes(1);

    rectSpy.mockReturnValue(new DOMRect(12.3, 34.4, 480.2, 320.3));
    ResizeObserverStub.instances[0].deliver();
    flushFrames();
    expect(onBounds).toHaveBeenCalledTimes(1);

    rectSpy.mockReturnValue(new DOMRect(12, 34, 480, 300));
    ResizeObserverStub.instances[0].deliver();
    flushFrames();
    expect(onBounds).toHaveBeenCalledTimes(2);
    expect(onBounds).toHaveBeenLastCalledWith({
      x: 12,
      y: 34,
      width: 480,
      height: 300,
    });
    rectSpy.mockRestore();
  });
});

describe("PreviewPanel title bar", () => {
  it("mounts the header as the shared Preview toolbar carrying the heading", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      throw new Error(`Unexpected command: ${cmd}`);
    });

    render(<PreviewPanel projectId={null} />);

    // v3 §6.1: `.preview-toolbar` is gone; the shared TitleBar primitive
    // (role="toolbar", aria-label "Preview") owns the row, and the card is
    // still labelled by the same heading, now living in its title slot.
    const bar = screen.getByRole("toolbar", { name: "Preview" });
    expect(bar).toBeTruthy();
    const heading = screen.getByRole("heading", { name: "Workspace preview" });
    expect(heading.id).toBe("preview-heading");
    expect(
      screen.getByRole("region", { name: "Workspace preview" }),
    ).toBeTruthy();
  });

  it("collapses the column through the always-pressed toggle", () => {
    const onTogglePreview = vi.fn();
    render(<PreviewPanel projectId={null} onTogglePreview={onTogglePreview} />);

    // The column is open exactly while this panel renders, so the toggle
    // reports pressed for the whole life of the panel (v3 §6.1).
    const toggle = screen.getByRole("button", { name: "Toggle preview" });
    expect(toggle.getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(toggle);
    expect(onTogglePreview).toHaveBeenCalledTimes(1);
  });

  it("keeps the collapse toggle last in the trailing slot", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "project_preview_status") {
        return statusFixture({
          state: "ready",
          revision: 1,
          url: "http://127.0.0.1:49312/",
          nextCursor: 1,
        });
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });

    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    render(<PreviewPanel projectId={SESSION} onTogglePreview={() => {}} />);
    await settleFirstRead();
    expect(screen.getByText("Ready")).toBeTruthy();

    // v3 §6.1 order: open, then the power button, then the toggle as the
    // very last control. (Power buttons name themselves by text; icon
    // buttons by aria-label.)
    const buttons = Array.from(
      screen
        .getByRole("toolbar", { name: "Preview" })
        .querySelectorAll("button"),
    ).map((button) => button.getAttribute("aria-label") ?? button.textContent);
    expect(buttons).toEqual(["Open in browser", "Stop", "Toggle preview"]);
  });
});

describe("PreviewPanel overlay fallback", () => {
  /** Poll fixture: a session stuck at `preparing`, whose viewport normally
   *  shows the hot-reload guidance line. */
  function preparingInvoke() {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "project_preview_status") {
        return statusFixture({
          state: "preparing",
          revision: 1,
          nextCursor: 1,
        });
      }
      throw new Error(`Unexpected command: ${cmd}`);
    });
  }

  it("shows the stored snapshot inside the viewport while an overlay is open", async () => {
    preparingInvoke();
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const { container } = render(
      <PreviewPanel
        projectId={SESSION}
        overlayOpen
        pocketName="habit-tracker"
        snapshotUrl="data:image/png;base64,AAAA"
      />,
    );
    await settleFirstRead();

    // v3 §9.2: the snapshot stands in for the hidden native pane…
    const snapshot = container.querySelector("img.preview-overlay-snapshot");
    expect(snapshot).toBeTruthy();
    expect(snapshot?.getAttribute("src")).toBe("data:image/png;base64,AAAA");
    // …and the starting affordance inside the viewport is gone…
    expect(container.querySelector(".preview-viewport .hint")).toBeNull();
    // …while the chrome above keeps working exactly as before.
    expect(screen.getByText("Preparing…")).toBeTruthy();
    expect(
      screen.getByText("No address while the preview is not live"),
    ).toBeTruthy();
  });

  it("falls back to the pocket name without a snapshot", async () => {
    preparingInvoke();
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const { container } = render(
      <PreviewPanel
        projectId={SESSION}
        overlayOpen
        pocketName="habit-tracker"
        snapshotUrl={null}
      />,
    );
    await settleFirstRead();

    // No honest snapshot, no picture: the name centred on plain --bg.
    const name = screen.getByText("habit-tracker");
    expect(name.className).toContain("preview-overlay-name");
    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector(".preview-viewport .hint")).toBeNull();
  });

  it("shows nothing when the overlay has neither snapshot nor name", () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      throw new Error(`Unexpected command: ${cmd}`);
    });

    const { container } = render(
      <PreviewPanel
        projectId={null}
        overlayOpen
        pocketName={null}
        snapshotUrl={null}
      />,
    );
    const viewport = container.querySelector(".preview-viewport");
    expect(viewport?.textContent).toBe("");
  });

  it("restores the normal affordances when no overlay is open", async () => {
    preparingInvoke();
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const { container } = render(<PreviewPanel projectId={SESSION} />);
    await settleFirstRead();

    // Default state untouched: the viewport guidance is there, no snapshot,
    // no name overlay.
    expect(screen.getByText(/Saved edits refresh automatically/)).toBeTruthy();
    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector(".preview-overlay-name")).toBeNull();
  });

  it("keeps the viewport mounted and reporting while the overlay hides the pane", () => {
    // The bounds contract survives the overlay (v3 §9.2): App stores these
    // reports while the pane is hidden and re-applies them on restore, so
    // the measuring element must never unmount or stop tracking.
    const rectSpy = vi
      .spyOn(Element.prototype, "getBoundingClientRect")
      .mockReturnValue(new DOMRect(10, 20, 500, 300));
    const onBounds = vi.fn();

    const view = render(
      <PreviewPanel
        projectId={null}
        onBounds={onBounds}
        overlayOpen
        pocketName="habit-tracker"
        snapshotUrl={null}
      />,
    );
    expect(view.container.querySelector(".preview-viewport")).toBeTruthy();
    expect(onBounds).toHaveBeenCalledTimes(1); // mount report stands

    // Resize under the overlay: the observer still tracks the rect.
    rectSpy.mockReturnValue(new DOMRect(10, 20, 460, 300));
    ResizeObserverStub.instances[0].deliver();
    flushFrames();
    expect(onBounds).toHaveBeenCalledTimes(2);
    expect(onBounds).toHaveBeenLastCalledWith({
      x: 10,
      y: 20,
      width: 460,
      height: 300,
    });

    // Closing the overlay restores the pane with a fresh report right away,
    // not on the next resize (v3 §9.3) — an immediate placement report, so
    // it is sent even though the rect has not moved again.
    view.rerender(
      <PreviewPanel
        projectId={null}
        onBounds={onBounds}
        overlayOpen={false}
        pocketName="habit-tracker"
        snapshotUrl={null}
      />,
    );
    expect(onBounds).toHaveBeenCalledTimes(3);
    expect(onBounds).toHaveBeenLastCalledWith({
      x: 10,
      y: 20,
      width: 460,
      height: 300,
    });
    rectSpy.mockRestore();
  });
});

/** Top-level rule bodies, comments and @media blocks stripped. */
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

describe("preview layout contract", () => {
  it("pins the rail grid, the growing viewport, and the collapsed rail", () => {
    // Chat, the 1px divider track and the preview rail (v3 §8): columns
    // only, at every width — the divider owns the boundary now, so the
    // chat pane carries no border of its own.
    expect(soleRuleBody(stylesCss, ".workspace")).toContain(
      "grid-template-columns: minmax(360px, 1fr) 1px 1.2fr;",
    );
    expect(soleRuleBody(stylesCss, ".column-divider")).toContain(
      "cursor: col-resize;",
    );
    // Collapsing the rail (the title row's toggle) drops the workspace to
    // the chat alone.
    expect(
      soleRuleBody(stylesCss, '.workspace[data-preview-collapsed="true"]'),
    ).toContain("grid-template-columns: minmax(0, 1fr);");
    // The reserved region grows with the rail and never shrinks under a
    // usable pane height — it is the rect the native pane is placed over.
    const viewport = soleRuleBody(previewCss, ".preview-viewport");
    expect(viewport).toContain("flex: 1;");
    expect(viewport).toContain("min-height: 220px;");
    // v3 §6.4: the pane fills the region edge to edge — no margins, no
    // padding, no frame; the strips' own bottom borders and the column
    // divider now frame the pocket.
    expect(viewport).toContain("margin: 0;");
    expect(viewport).toContain("padding: 0;");
    expect(viewport).toContain("border: 0;");
    expect(viewport).toContain("background: var(--bg);");
    // Columns never stack: the shell has no responsive reflow at all, so
    // styles.css must not carry any max-width breakpoint (the old 1200px
    // wrap-under and 720px document-stack are gone by contract).
    expect(stylesCss).not.toMatch(/@media[^{]*max-width/);
    // v3 §6.2: the URL strip spans the full width — 32px tall, its own
    // bottom border, no margins — and the address ellipsises instead of
    // wrapping, so a long loopback URL can never blow out the row.
    const urlBar = soleRuleBody(previewCss, ".preview-url-bar");
    expect(urlBar).toContain("background: var(--surface);");
    expect(urlBar).toContain("height: var(--space-32);");
    expect(urlBar).toContain("padding: 0 var(--space-12);");
    expect(urlBar).toContain("border-bottom: 1px solid var(--preview-frame);");
    expect(urlBar).not.toContain("margin");
    expect(soleRuleBody(previewCss, ".preview-url")).toContain(
      "text-overflow: ellipsis;",
    );
    // v3 §6.3: the status lane is a full-width row when not empty — inset
    // by padding, closed by its own bottom border, no margins.
    const status = soleRuleBody(previewCss, ".preview-status:not(:empty)");
    expect(status).toContain("padding: var(--space-8) var(--space-12);");
    expect(status).toContain("border-bottom: 1px solid var(--border);");
    expect(status).not.toContain("margin");
    // v3 §6.1/§10: the old wrapping toolbar rule is gone — the shared
    // TitleBar owns the header row and nothing in this pane wraps.
    expect(
      topLevelRules(previewCss).flatMap((rule) => rule.selectors),
    ).not.toContain(".preview-toolbar");
    expect(previewCss).not.toMatch(/flex-wrap/);
    // v3 §9.2: the overlay snapshot fills the reserved rect, cover-cropped.
    expect(soleRuleBody(previewCss, ".preview-overlay-snapshot")).toContain(
      "object-fit: cover;",
    );
    // Flat by contract (v2 §4): nothing in this pane is rounded.
    expect(previewCss).not.toMatch(/border-radius/);
  });
});
