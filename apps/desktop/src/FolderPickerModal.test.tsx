import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import FolderPickerModal from "./FolderPickerModal";
import { invoke, type DirectoryListing } from "./api";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function stubBrowse(listings: Record<string, DirectoryListing>) {
  const fetchMock = vi.fn().mockImplementation(async (url, init) => {
    if (url === "/api/rpc/browse_directory") {
      const body = JSON.parse(String(init?.body ?? "{}")) as {
        path?: string | null;
      };
      const key = body.path ?? "__default__";
      const result = listings[key];
      if (!result) {
        return new Response(
          JSON.stringify({ ok: false, error: `Unknown path: ${key}` }),
          { status: 400, headers: { "Content-Type": "application/json" } },
        );
      }
      return new Response(JSON.stringify({ ok: true, result }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      });
    }
    return new Response(JSON.stringify({ ok: true, result: { id: "p1" } }), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    });
  });
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

describe("FolderPickerModal", () => {
  it("shows a live destination preview with the chosen folder name while keeping the path input navigable", async () => {
    const fetchMock = stubBrowse({
      __default__: {
        currentPath: "~/Code",
        parentPath: "~",
        hasPocketYaml: false,
        entries: [
          { name: "personal", path: "~/Code/personal", hasPocketYaml: false },
          { name: "scratch", path: "~/Code/scratch", hasPocketYaml: false },
        ],
        truncated: false,
      },
      "~/Code/personal": {
        currentPath: "~/Code/personal",
        parentPath: "~/Code",
        hasPocketYaml: false,
        entries: [],
        truncated: false,
      },
    });

    render(<FolderPickerModal />);

    const pending = invoke("create_development_project", {
      name: "Recipe Box",
      folderName: "recipe-box",
    });

    const pathInput =
      await screen.findByLabelText<HTMLInputElement>("Folder path");
    await waitFor(() => expect(pathInput.value).toBe("~/Code"));

    expect(screen.getByText("Will create")).toBeTruthy();
    expect(
      screen.getByText(
        (_, node) =>
          node?.tagName === "CODE" && node.textContent === "~/Code/recipe-box",
      ),
    ).toBeTruthy();

    // Navigate into a subfolder and confirm the preview updates without appending to the input.
    fireEvent.click(screen.getByText("personal"));
    await waitFor(() => expect(pathInput.value).toBe("~/Code/personal"));
    expect(
      screen.getByText(
        (_, node) =>
          node?.tagName === "CODE" &&
          node.textContent === "~/Code/personal/recipe-box",
      ),
    ).toBeTruthy();

    // Confirming still submits the chosen parent folder, not the combined path.
    fireEvent.click(screen.getByRole("button", { name: "Create here" }));
    await pending;

    expect(fetchMock).toHaveBeenCalledWith(
      "/api/rpc/create_development_project",
      expect.objectContaining({
        body: JSON.stringify({
          name: "Recipe Box",
          folderName: "recipe-box",
          parentPath: "~/Code/personal",
        }),
      }),
    );
  });

  it("warns and disables confirmation when a folder with the chosen name already exists", async () => {
    stubBrowse({
      __default__: {
        currentPath: "~/Code",
        parentPath: "~",
        hasPocketYaml: false,
        entries: [
          {
            name: "recipe-box",
            path: "~/Code/recipe-box",
            hasPocketYaml: false,
          },
          { name: "scratch", path: "~/Code/scratch", hasPocketYaml: false },
        ],
        truncated: false,
      },
      "~/Code/scratch": {
        currentPath: "~/Code/scratch",
        parentPath: "~/Code",
        hasPocketYaml: false,
        entries: [],
        truncated: false,
      },
    });

    render(<FolderPickerModal />);

    const pending = invoke("create_development_project", {
      name: "Recipe Box",
      folderName: "recipe-box",
    });

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("recipe-box");
    expect(
      screen.getByRole<HTMLButtonElement>("button", { name: "Create here" })
        .disabled,
    ).toBe(true);

    // Navigating into a folder without the collision clears the warning and re-enables confirmation.
    fireEvent.click(screen.getByText("scratch"));
    await waitFor(() => expect(screen.queryByRole("alert")).toBeNull());
    expect(
      screen.getByRole<HTMLButtonElement>("button", { name: "Create here" })
        .disabled,
    ).toBe(false);

    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(await pending).toBeNull();
  });

  it("omits the destination preview for folder picks that do not create a subfolder", async () => {
    stubBrowse({
      __default__: {
        currentPath: "~/Code",
        parentPath: "~",
        hasPocketYaml: false,
        entries: [],
        truncated: false,
      },
    });

    render(<FolderPickerModal />);

    const pending = invoke("link_development_folder", {});

    const pathInput =
      await screen.findByLabelText<HTMLInputElement>("Folder path");
    await waitFor(() => expect(pathInput.value).toBe("~/Code"));
    expect(screen.queryByText("Will create")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(await pending).toBeNull();
  });
});
