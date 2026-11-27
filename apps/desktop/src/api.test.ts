import { afterEach, describe, expect, it, vi } from "vitest";
import { invoke, registerFolderPicker } from "./api";

describe("api RPC bridge", () => {
  afterEach(() => {
    registerFolderPicker(null);
    vi.restoreAllMocks();
  });

  it("posts JSON arguments to /api/rpc/<command> with credentials and returns result", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ ok: true, result: { state: "ready" } }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    vi.stubGlobal("fetch", fetchMock);

    const res = await invoke<{ state: string }>("detect_runtime");
    expect(res).toEqual({ state: "ready" });
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/rpc/detect_runtime",
      expect.objectContaining({
        method: "POST",
        credentials: "include",
        body: "{}",
      }),
    );
  });

  it("throws the daemon error message on non-ok RPC responses", async () => {
    vi.stubGlobal(
      "fetch",
      vi
        .fn()
        .mockResolvedValue(
          new Response(
            JSON.stringify({ ok: false, error: "Podman is not running." }),
            { status: 400, headers: { "Content-Type": "application/json" } },
          ),
        ),
    );

    await expect(invoke("start_project", { id: "abc" })).rejects.toThrow(
      "Podman is not running.",
    );
  });

  it("requests a folder via the registered picker before linking a folder", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ ok: true, result: { id: "p1" } }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    vi.stubGlobal("fetch", fetchMock);
    registerFolderPicker(async () => "/tmp/my-pocket");

    const project = await invoke<{ id: string }>("link_development_folder", {});
    expect(project).toEqual({ id: "p1" });
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/rpc/link_development_folder",
      expect.objectContaining({
        body: JSON.stringify({ folderPath: "/tmp/my-pocket" }),
      }),
    );
  });

  it("passes folderName as createSubfolder to the folder picker when creating a project", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ ok: true, result: { id: "p1" } }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    vi.stubGlobal("fetch", fetchMock);
    const picker = vi.fn().mockResolvedValue("~/Code");
    registerFolderPicker(picker);

    const project = await invoke<{ id: string }>("create_development_project", {
      name: "Recipe Box",
      folderName: "recipe-box",
    });

    expect(project).toEqual({ id: "p1" });
    expect(picker).toHaveBeenCalledWith({
      title: "Choose parent folder",
      confirmLabel: "Create here",
      createSubfolder: "recipe-box",
    });
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/rpc/create_development_project",
      expect.objectContaining({
        body: JSON.stringify({
          name: "Recipe Box",
          folderName: "recipe-box",
          parentPath: "~/Code",
        }),
      }),
    );
  });

  it("requests a folder via the registered picker before inspecting a folder for pocketize", async () => {
    const fetchMock = vi.fn().mockResolvedValue(
      new Response(
        JSON.stringify({
          ok: true,
          result: { folderPath: "~/Code/budget", alreadyPocket: false },
        }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      ),
    );
    vi.stubGlobal("fetch", fetchMock);
    const picker = vi.fn().mockResolvedValue("~/Code/budget");
    registerFolderPicker(picker);

    const info = await invoke<{ folderPath: string; alreadyPocket: boolean }>(
      "inspect_pocketize_folder",
      {},
    );
    expect(info).toEqual({
      folderPath: "~/Code/budget",
      alreadyPocket: false,
    });
    expect(picker).toHaveBeenCalledWith(
      expect.objectContaining({
        title: "Choose project folder",
        confirmLabel: "Choose folder",
        preferPocketFolder: true,
      }),
    );
    expect(fetchMock).toHaveBeenCalledWith(
      "/api/rpc/inspect_pocketize_folder",
      expect.objectContaining({
        body: JSON.stringify({ folderPath: "~/Code/budget" }),
      }),
    );
  });

  it("returns null without hitting the daemon when folder picking is cancelled", async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal("fetch", fetchMock);
    registerFolderPicker(async () => null);

    const result = await invoke("create_development_project", {
      name: "Demo",
      folderName: "demo",
    });
    expect(result).toBeNull();
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it("opens verified URLs returned by open_project and project_preview_open in a new browser tab", async () => {
    const openSpy = vi.spyOn(window, "open").mockReturnValue(null);
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        new Response(
          JSON.stringify({
            ok: true,
            result: "https://cloud-vm.tail00000.ts.net:41234",
          }),
          { status: 200, headers: { "Content-Type": "application/json" } },
        ),
      ),
    );

    await invoke("project_preview_open", { projectId: "a".repeat(32) });
    expect(openSpy).toHaveBeenCalledWith(
      "https://cloud-vm.tail00000.ts.net:41234",
      "_blank",
      "noopener,noreferrer",
    );
  });
});
