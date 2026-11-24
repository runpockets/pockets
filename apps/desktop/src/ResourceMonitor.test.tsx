import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  within,
} from "@testing-library/react";
import ResourceMonitor from "./ResourceMonitor";
import { invoke } from "./api";

// The panel only ever talks to the native backend through `invoke`; the real
// module would fail outside Tauri, so mock the whole module.
vi.mock("./api", () => ({ invoke: vi.fn() }));

const invokeMock = vi.mocked(invoke);

const GIB = 1024 ** 3;
const MIB = 1024 ** 2;
const SAMPLED_AT = new Date(2026, 0, 15, 9, 8, 7).getTime();

/** Snapshot shaped like the native `resource_snapshot` payload. */
function snapshotFixture(overrides: Record<string, unknown> = {}) {
  return {
    sampledAtMs: SAMPLED_AT,
    pockets: [
      {
        id: "p-1",
        name: "Notes",
        state: "running",
        cpuPercent: 120,
        memoryBytes: 512 * MIB,
      },
      {
        id: "p-2",
        name: "Timer",
        state: "stopped",
        cpuPercent: null,
        memoryBytes: null,
      },
      {
        id: "p-3",
        name: "Ghost",
        state: "unavailable",
        cpuPercent: null,
        memoryBytes: null,
      },
    ],
    totals: { cpuPercent: 120, memoryBytes: 512 * MIB },
    warnings: [],
    ...overrides,
  };
}

/** The combined "Pockets total" scope card, addressed by its aria-label. */
function totalsCard(): HTMLElement {
  return screen.getByRole("region", { name: "Pockets total usage" });
}

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

const visibilityDescriptor = Object.getOwnPropertyDescriptor(
  Object.getPrototypeOf(document) as object,
  "visibilityState",
);

/** Flips `document.visibilityState` and announces the change. */
function setVisibility(value: "visible" | "hidden") {
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    get: () => value,
  });
  act(() => {
    document.dispatchEvent(new Event("visibilitychange"));
  });
}

function openMonitor() {
  fireEvent.click(
    screen.getByRole("button", { name: "Open Resource Monitor" }),
  );
}

function closeMonitor() {
  fireEvent.click(screen.getByRole("button", { name: "Close" }));
}

beforeEach(() => {
  invokeMock.mockReset();
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  if (visibilityDescriptor) {
    Object.defineProperty(document, "visibilityState", visibilityDescriptor);
  }
});

describe("ResourceMonitor closed by default", () => {
  it("requests nothing until opened, then invokes resource_snapshot with no arguments", async () => {
    // Only the 5s refresh timer is virtual; nothing may fire while closed.
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    invokeMock.mockImplementation(async () => snapshotFixture());

    render(<ResourceMonitor />);
    // The closed description is Pocket-only, with no whole-computer claims.
    expect(
      screen.getByText(/Pockets this app manages\. Nothing is sampled/),
    ).toBeTruthy();
    expect(screen.queryByText(/this computer/i)).toBeNull();
    expect(screen.queryByText(/Podman machine/)).toBeNull();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(invokeMock).not.toHaveBeenCalled();
    expect(screen.queryByRole("table")).toBeNull();

    openMonitor();
    expect(invokeMock).toHaveBeenCalledTimes(1);
    // Exact argument list: the webview sends no payload with the request.
    expect(invokeMock).toHaveBeenCalledWith("resource_snapshot");
  });
});

describe("ResourceMonitor rendering", () => {
  it("renders the Pockets total and per-Pocket rows with accurate labels", async () => {
    invokeMock.mockImplementation(async () => snapshotFixture());

    const { container } = render(<ResourceMonitor />);
    openMonitor();
    await screen.findByRole("table");

    // The only scope card is the combined Pockets total.
    const scopes = Array.from(
      container.querySelectorAll<HTMLElement>(".resource-scope"),
    );
    expect(scopes).toHaveLength(1);
    expect(scopes[0].textContent).toContain("Pockets total");
    expect(within(scopes[0]).getByText("CPU")).toBeTruthy();
    expect(within(totalsCard()).getByText("120%")).toBeTruthy();
    expect(within(totalsCard()).getByText("512.0 MiB")).toBeTruthy();

    // Host and Podman machine cards are gone, in content and in structure.
    expect(screen.queryByText("Host")).toBeNull();
    expect(screen.queryByText("Podman machine")).toBeNull();
    expect(screen.queryByRole("region", { name: "Host usage" })).toBeNull();
    expect(
      screen.queryByRole("region", { name: "Podman machine usage" }),
    ).toBeNull();
    expect(screen.queryByText("This whole computer")).toBeNull();
    expect(screen.queryByText("CPU since boot")).toBeNull();

    // Honest coverage note: the total is the managed containers only.
    expect(scopes[0].textContent).toMatch(/only the managed containers/);
    expect(scopes[0].textContent).toMatch(/not the Desktop app/);
    // The removed cross-scope overhead explanation stays gone.
    expect(screen.queryByText(/not additive/)).toBeNull();
    expect(screen.queryByText(/does not isolate/)).toBeNull();

    // Per-Pocket rows: status plus metrics for every lifecycle state.
    const notesRow = screen.getByRole("row", { name: /Notes/ });
    expect(within(notesRow).getByText("Running")).toBeTruthy();
    expect(within(notesRow).getByText("120%")).toBeTruthy();
    expect(within(notesRow).getByText("512.0 MiB")).toBeTruthy();
    const timerRow = screen.getByRole("row", { name: /Timer/ });
    expect(within(timerRow).getByText("Stopped")).toBeTruthy();
    expect(
      within(timerRow).getAllByText("Unavailable", {
        selector: ".resource-sr-only",
      }),
    ).toHaveLength(2);
    const ghostRow = screen.getByRole("row", { name: /Ghost/ });
    expect(
      within(ghostRow).getByText("Unavailable", { selector: ".badge" }),
    ).toBeTruthy();

    // Honest CPU semantics: per-core scale and single-shot Pocket readings.
    expect(screen.getByText("(100% = one core)")).toBeTruthy();
    expect(screen.getByText(/may reflect the average since that/)).toBeTruthy();

    // Fresh-sample indicator.
    expect(
      screen.getByText(`Updated ${new Date(SAMPLED_AT).toLocaleTimeString()}`),
    ).toBeTruthy();
    expect(screen.queryByText(/Nothing is sampled/)).toBeNull();
  });

  it("renders unavailable nulls as em dashes, never as zero", async () => {
    invokeMock.mockImplementation(async () =>
      snapshotFixture({
        pockets: [
          {
            id: "p-1",
            name: "Notes",
            state: "running",
            cpuPercent: null,
            memoryBytes: null,
          },
        ],
        totals: { cpuPercent: null, memoryBytes: 3 * GIB },
      }),
    );

    const { container } = render(<ResourceMonitor />);
    openMonitor();
    await screen.findByRole("table");

    // Nulls become "—" announced as "Unavailable" — never 0 or "0 B".
    // Two Pocket fields plus the totals CPU are null; totals memory is known.
    expect(container.querySelectorAll(".resource-dash")).toHaveLength(3);
    expect(
      screen.getAllByText("Unavailable", { selector: ".resource-sr-only" }),
    ).toHaveLength(3);
    expect(screen.queryByText("0%")).toBeNull();
    expect(screen.queryByText("0 B")).toBeNull();
    expect(container.textContent).toContain("—");

    // Memory without a known total shows only the used amount.
    expect(within(totalsCard()).getByText("3.0 GiB")).toBeTruthy();
  });

  it("reports a failed first sample explicitly, with no zeros", async () => {
    invokeMock.mockImplementation(async () => {
      throw new Error("resource_snapshot is unavailable");
    });

    const { container } = render(<ResourceMonitor />);
    openMonitor();

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("resource_snapshot is unavailable");
    expect(screen.getByText("No sample collected yet.")).toBeTruthy();
    expect(screen.queryByRole("table")).toBeNull();
    // Scope scaffolding still shows unavailable dashes, never 0.
    expect(container.querySelectorAll(".resource-dash")).toHaveLength(2);
    expect(screen.queryByText("0%")).toBeNull();
    expect(screen.queryByText("0 B")).toBeNull();
  });

  it("surfaces backend warnings with the sample", async () => {
    invokeMock.mockImplementation(async () =>
      snapshotFixture({
        warnings: [
          "guest stats partial: perf unavailable",
          "memory total missing",
        ],
      }),
    );

    render(<ResourceMonitor />);
    openMonitor();

    const status = await screen.findByRole("status");
    expect(within(status).getAllByRole("listitem")).toHaveLength(2);
    expect(
      screen.getByText("guest stats partial: perf unavailable"),
    ).toBeTruthy();
  });
});

describe("ResourceMonitor refresh lifecycle", () => {
  it("keeps values visibly stale on a failed refresh and recovers on the next success", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    let calls = 0;
    invokeMock.mockImplementation(async () => {
      calls += 1;
      if (calls === 2) throw new Error("stats pipe closed");
      return snapshotFixture();
    });

    render(<ResourceMonitor />);
    openMonitor();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(within(totalsCard()).getByText("120%")).toBeTruthy();

    // Refresh fails: the error is explicit and the kept values are marked.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000);
    });
    expect(invokeMock).toHaveBeenCalledTimes(2);
    expect(screen.getByRole("alert").textContent).toContain(
      "stats pipe closed",
    );
    expect(screen.getByText("Stale")).toBeTruthy();
    expect(within(totalsCard()).getByText("120%")).toBeTruthy();

    // The loop keeps running and a later success clears the stale state.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000);
    });
    expect(invokeMock).toHaveBeenCalledTimes(3);
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.queryByText("Stale")).toBeNull();
    expect(within(totalsCard()).getByText("120%")).toBeTruthy();
  });

  it("stops refreshing when closed", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    invokeMock.mockImplementation(async () => snapshotFixture());

    render(<ResourceMonitor />);
    openMonitor();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000);
    });
    expect(invokeMock).toHaveBeenCalledTimes(2);

    closeMonitor();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(60_000);
    });
    expect(invokeMock).toHaveBeenCalledTimes(2);
  });

  it("discards a reply that lands after closing and refetches on reopen", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const inFlight: Array<ReturnType<typeof deferred>> = [];
    invokeMock.mockImplementation(() => {
      const call = deferred();
      inFlight.push(call);
      return call.promise;
    });

    render(<ResourceMonitor />);
    openMonitor();
    // Closing stays available while the request is still pending.
    expect(screen.getByRole("button", { name: "Close" })).toBeTruthy();
    closeMonitor();

    await act(async () => {
      inFlight[0].resolve(snapshotFixture());
    });
    // The late reply is dropped: nothing is rendered and nothing errored.
    expect(screen.queryByRole("table")).toBeNull();
    expect(screen.queryByText("120%")).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
    expect(invokeMock).toHaveBeenCalledTimes(1);

    // Reopen starts a fresh request rather than resurrecting cached state.
    openMonitor();
    expect(invokeMock).toHaveBeenCalledTimes(2);
    await act(async () => {
      inFlight[1].resolve(snapshotFixture());
    });
    expect(within(totalsCard()).getByText("120%")).toBeTruthy();
  });

  it("retires the previous sample on reopen until a fresh one arrives", async () => {
    invokeMock.mockImplementation(async () => snapshotFixture());

    render(<ResourceMonitor />);
    openMonitor();
    await screen.findByText("Ghost");

    closeMonitor();
    expect(screen.queryByText("Ghost")).toBeNull();

    openMonitor();
    // Never replays the old snapshot: the reopened card shows loading.
    expect(screen.getByText("Collecting the first sample…")).toBeTruthy();
    expect(screen.queryByText("Ghost")).toBeNull();
    await screen.findByText("Ghost");
    expect(invokeMock).toHaveBeenCalledTimes(2);
  });

  it("pauses while the page is hidden and samples fresh on return", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    invokeMock.mockImplementation(async () => snapshotFixture());

    render(<ResourceMonitor />);
    openMonitor();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);

    setVisibility("hidden");
    await act(async () => {
      await vi.advanceTimersByTimeAsync(30_000);
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);

    // Back on screen: one fresh sample immediately, then the normal cadence.
    setVisibility("visible");
    expect(invokeMock).toHaveBeenCalledTimes(2);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000);
    });
    expect(invokeMock).toHaveBeenCalledTimes(3);
  });

  it("never overlaps requests when a reply outlives the refresh interval", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const inFlight: Array<ReturnType<typeof deferred>> = [];
    invokeMock.mockImplementation(() => {
      const call = deferred();
      inFlight.push(call);
      return call.promise;
    });

    render(<ResourceMonitor />);
    openMonitor();
    // The first reply is stuck: no second request may start, ever.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(20_000);
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);

    // The 5s cadence is armed only after the previous reply settled.
    await act(async () => {
      inFlight[0].resolve(snapshotFixture());
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(4_999);
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1);
    });
    expect(invokeMock).toHaveBeenCalledTimes(2);
    await act(async () => {
      inFlight[1].resolve(snapshotFixture());
    });
  });

  it("never overlaps or loses the loop across a close and immediate reopen", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const inFlight: Array<ReturnType<typeof deferred>> = [];
    invokeMock.mockImplementation(() => {
      const call = deferred();
      inFlight.push(call);
      return call.promise;
    });

    render(<ResourceMonitor />);
    openMonitor();
    closeMonitor();
    openMonitor();
    // The old session's request is still in flight: the new session defers
    // instead of firing a second request.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(20_000);
    });
    expect(invokeMock).toHaveBeenCalledTimes(1);

    // When the old reply settles (discarded), the open session samples now.
    await act(async () => {
      inFlight[0].resolve(snapshotFixture());
    });
    expect(invokeMock).toHaveBeenCalledTimes(2);
    expect(screen.queryByText("Ghost")).toBeNull();
    await act(async () => {
      inFlight[1].resolve(snapshotFixture());
    });
    expect(within(totalsCard()).getByText("120%")).toBeTruthy();

    // And the normal cadence resumed for the live session.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000);
    });
    expect(invokeMock).toHaveBeenCalledTimes(3);
  });
});
