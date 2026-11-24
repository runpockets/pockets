import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { invoke } from "./api";
import DevelopmentPanel from "./DevelopmentPanel";

vi.mock("./api", () => ({ invoke: vi.fn() }));
const invokeMock = vi.mocked(invoke);
const linked = {
  linked: true,
  folder: "/projects/tracker",
  available: true,
  error: null,
};
const unlinked = { linked: false, folder: null, available: false, error: null };

async function expand() {
  const details = screen.getByText("External development").closest("details")!;
  details.open = true;
  fireEvent(details, new Event("toggle"));
  await waitFor(() =>
    expect(invokeMock).toHaveBeenCalledWith("development_status", {
      id: "project-one",
    }),
  );
}

describe("External development", () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });
  afterEach(cleanup);

  it("loads only when expanded and unlinks the linked folder without building", async () => {
    const changed = vi.fn();
    let isLinked = true;
    invokeMock.mockImplementation(async (command) => {
      if (command === "development_status") return isLinked ? linked : unlinked;
      if (command === "unlink_development_folder") {
        isLinked = false;
        return undefined;
      }
      throw new Error(`Unexpected command: ${command}`);
    });
    render(
      <DevelopmentPanel
        projectId="project-one"
        disabled={false}
        onSourceChanged={changed}
      />,
    );
    expect(invokeMock).not.toHaveBeenCalled();
    await expand();
    expect(screen.queryByRole("button", { name: "Sync source" })).toBeNull();
    fireEvent.click(
      await screen.findByRole("button", { name: "Unlink folder" }),
    );
    await waitFor(() => expect(changed).toHaveBeenCalledOnce());
    expect(invokeMock).toHaveBeenCalledWith("unlink_development_folder", {
      id: "project-one",
    });
    expect(
      invokeMock.mock.calls.some(
        ([command]) =>
          command === "build_project" || command === "start_project",
      ),
    ).toBe(false);
  });

  it("treats picker cancellation as no change", async () => {
    const changed = vi.fn();
    invokeMock.mockImplementation(async (command) =>
      command === "development_status" ? unlinked : null,
    );
    render(
      <DevelopmentPanel
        projectId="project-one"
        disabled={false}
        onSourceChanged={changed}
      />,
    );
    await expand();
    fireEvent.click(await screen.findByText("Link development folder"));
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("link_development_folder", {
        id: "project-one",
      }),
    );
    await waitFor(() => expect(screen.queryByText("Working…")).toBeNull());
    expect(changed).not.toHaveBeenCalled();
    expect(screen.queryByText("Source synced")).toBeNull();
  });

  it("exports a snapshot for the same project and refreshes the link", async () => {
    const changed = vi.fn();
    let exported = false;
    invokeMock.mockImplementation(async (command) => {
      if (command === "development_status") return exported ? linked : unlinked;
      if (command === "export_development_project") {
        exported = true;
        return { id: "project-one" };
      }
      throw new Error(`Unexpected command: ${command}`);
    });
    render(
      <DevelopmentPanel
        projectId="project-one"
        disabled={false}
        onSourceChanged={changed}
      />,
    );
    await expand();
    fireEvent.click(
      await screen.findByRole("button", { name: "Export saved source…" }),
    );
    expect(await screen.findByText("Folder connected")).toBeTruthy();
    expect(changed).toHaveBeenCalledOnce();
    expect(invokeMock).toHaveBeenCalledWith("export_development_project", {
      id: "project-one",
    });
  });

  it("opens and copies the linked folder from icons beside its path", async () => {
    const changed = vi.fn();
    invokeMock.mockImplementation(async (command) =>
      command === "development_status" ? linked : undefined,
    );
    render(
      <DevelopmentPanel
        projectId="project-one"
        disabled={false}
        onSourceChanged={changed}
      />,
    );
    await expand();
    const row = within((await screen.findByText(linked.folder)).parentElement!);
    const open = row.getByRole<HTMLButtonElement>("button", {
      name: "Open folder",
    });
    const copy = row.getByRole<HTMLButtonElement>("button", {
      name: "Copy folder path",
    });
    fireEvent.click(open);
    expect(invokeMock).toHaveBeenCalledWith("open_development_folder", {
      id: "project-one",
    });
    await waitFor(() => expect(copy.disabled).toBe(false));
    fireEvent.click(copy);
    expect(await screen.findByText("Folder path copied.")).toBeTruthy();
    expect(invokeMock).toHaveBeenCalledWith("copy_development_location", {
      id: "project-one",
    });
    expect(changed).not.toHaveBeenCalled();
  });

  it("keeps relink and unlink available when the folder is missing", async () => {
    invokeMock.mockResolvedValue({
      ...linked,
      available: false,
      error: "Locate the folder again.",
    });
    render(
      <DevelopmentPanel
        projectId="project-one"
        disabled={false}
        onSourceChanged={vi.fn()}
      />,
    );
    await expand();
    const open = await screen.findByRole("button", { name: "Open folder" });
    expect(open.hasAttribute("disabled")).toBe(true);
    expect(
      screen
        .getByRole("button", { name: "Copy folder path" })
        .hasAttribute("disabled"),
    ).toBe(true);
    expect(
      screen
        .getByRole("button", { name: "Relink folder" })
        .hasAttribute("disabled"),
    ).toBe(false);
    expect(
      screen
        .getByRole("button", { name: "Unlink folder" })
        .hasAttribute("disabled"),
    ).toBe(false);
    expect(screen.getByText("Locate the folder again.")).toBeTruthy();
  });

  it("surfaces errors from development folder actions", async () => {
    const changed = vi.fn();
    invokeMock.mockImplementation(async (command) => {
      if (command === "development_status") return linked;
      throw new Error("Could not unlink the folder.");
    });
    render(
      <DevelopmentPanel
        projectId="project-one"
        disabled={false}
        onSourceChanged={changed}
      />,
    );
    await expand();
    fireEvent.click(
      await screen.findByRole("button", { name: "Unlink folder" }),
    );
    expect(await screen.findByText(/Could not unlink the folder/)).toBeTruthy();
    expect(changed).not.toHaveBeenCalled();
  });

  it("discards a late status reply after switching projects", async () => {
    let resolveFirst!: (value: unknown) => void;
    invokeMock.mockImplementation(async (_command, args) => {
      if ((args as { id: string }).id === "project-one") {
        return new Promise((resolve) => {
          resolveFirst = resolve;
        });
      }
      return unlinked;
    });
    const props = { disabled: false, onSourceChanged: vi.fn() };
    const view = render(
      <DevelopmentPanel {...props} projectId="project-one" />,
    );
    await expand();
    view.rerender(<DevelopmentPanel {...props} projectId="project-two" />);
    await screen.findByText(/Export this Pocket’s saved source/);
    await act(async () => resolveFirst(linked));
    expect(screen.queryByText("/projects/tracker")).toBeNull();
  });
});
