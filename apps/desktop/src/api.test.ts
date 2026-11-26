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
      vi.fn().mockResolvedValue(
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
