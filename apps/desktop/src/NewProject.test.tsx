import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { invoke } from "./api";
import NewProject from "./NewProject";

vi.mock("./api", () => ({ invoke: vi.fn() }));
const native = vi.mocked(invoke);
const project = {
  id: "created-project",
  manifest: { metadata: { name: "My portfolio" } },
};

function renderForm() {
  const onCreated = vi.fn();
  const view = render(
    <NewProject
      sidebarCollapsed={false}
      previewCollapsed={false}
      onToggleSidebar={vi.fn()}
      onTogglePreview={vi.fn()}
      onCreated={onCreated}
    />,
  );
  return { ...view, onCreated };
}

describe("New project", () => {
  beforeEach(() => {
    native.mockReset();
  });
  afterEach(cleanup);

  it("creates a linked project with a brief through the native location picker without starting runtime or AI", async () => {
    native.mockResolvedValue(project);
    const { onCreated } = renderForm();
    expect(native).not.toHaveBeenCalled();
    fireEvent.change(screen.getByLabelText("Project name"), {
      target: { value: "My portfolio" },
    });
    expect(
      (screen.getByLabelText("Folder name") as HTMLInputElement).value,
    ).toBe("my-portfolio");
    fireEvent.change(screen.getByLabelText("Project brief (optional)"), {
      target: { value: "Holdings and clearly labeled sample prices" },
    });
    fireEvent.click(
      screen.getByRole("button", { name: "Choose location & create" }),
    );
    await waitFor(() =>
      expect(onCreated).toHaveBeenCalledWith({
        ...project,
        developmentLinked: true,
      }),
    );
    expect(native.mock.calls).toEqual([
      [
        "create_development_project",
        {
          name: "My portfolio",
          folderName: "my-portfolio",
          brief: "Holdings and clearly labeled sample prices",
        },
      ],
    ]);
  });

  it("preserves form input on picker cancellation and respects an edited folder name", async () => {
    native.mockResolvedValue(null);
    const { onCreated } = renderForm();
    fireEvent.change(screen.getByLabelText("Project name"), {
      target: { value: "First name" },
    });
    fireEvent.change(screen.getByLabelText("Folder name"), {
      target: { value: "custom-folder" },
    });
    fireEvent.change(screen.getByLabelText("Project name"), {
      target: { value: "Second name" },
    });
    fireEvent.click(
      screen.getByRole("button", { name: "Choose location & create" }),
    );
    await waitFor(() =>
      expect(
        screen
          .getByRole("button", { name: "Choose location & create" })
          .hasAttribute("disabled"),
      ).toBe(false),
    );
    expect(
      (screen.getByLabelText("Project name") as HTMLInputElement).value,
    ).toBe("Second name");
    expect(
      (screen.getByLabelText("Folder name") as HTMLInputElement).value,
    ).toBe("custom-folder");
    expect(screen.queryByRole("alert")).toBeNull();
    expect(onCreated).not.toHaveBeenCalled();
  });

  it("shows creation errors and prevents duplicate submission while the picker is pending", async () => {
    let reject!: (error: Error) => void;
    native.mockImplementation(
      () =>
        new Promise((_resolve, fail) => {
          reject = fail;
        }),
    );
    renderForm();
    fireEvent.change(screen.getByLabelText("Project name"), {
      target: { value: "Tracker" },
    });
    const form = screen.getByLabelText("Project name").closest("form")!;
    fireEvent.submit(form);
    fireEvent.submit(form);
    expect(native).toHaveBeenCalledOnce();
    await act(async () =>
      reject(new Error("That folder already exists. Use Link project.")),
    );
    expect((await screen.findByRole("alert")).textContent).toContain(
      "already exists",
    );
    expect(
      (screen.getByLabelText("Project name") as HTMLInputElement).value,
    ).toBe("Tracker");
  });

  it("links existing source without requiring creation fields, and reports registration after unmount", async () => {
    let resolve!: (value: unknown) => void;
    native.mockImplementation(
      () =>
        new Promise((done) => {
          resolve = done;
        }),
    );
    const { onCreated, unmount } = renderForm();
    fireEvent.click(
      screen.getByRole("button", { name: "Link existing project" }),
    );
    expect(native).toHaveBeenCalledWith("link_development_folder", {});
    unmount();
    await act(async () => resolve(project));
    expect(onCreated).toHaveBeenCalledWith({
      ...project,
      developmentLinked: true,
    });
  });

  it("explains invalid folder slugs inline while keeping submission disabled", () => {
    renderForm();
    fireEvent.change(screen.getByLabelText("Project name"), {
      target: { value: "My App" },
    });
    const folderInput = screen.getByLabelText<HTMLInputElement>("Folder name");
    fireEvent.change(folderInput, { target: { value: "Bad Folder!" } });
    expect(folderInput.getAttribute("aria-invalid")).toBe("true");
    expect(
      screen.getByText(/Use lowercase letters, numbers, and single hyphens/),
    ).toBeTruthy();
    expect(
      screen
        .getByRole("button", { name: "Choose location & create" })
        .hasAttribute("disabled"),
    ).toBe(true);
  });

  it("adds a Pocket from a GitHub repository", async () => {
    native.mockResolvedValueOnce(project);
    const { onCreated } = renderForm();
    fireEvent.change(screen.getByLabelText("GitHub repository"), {
      target: { value: "runpockets/crypto-tracker" },
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Add from GitHub" }));
    });
    expect(native).toHaveBeenCalledWith("add_github_pocket", {
      source: "runpockets/crypto-tracker",
    });
    expect(onCreated).toHaveBeenCalledWith({
      ...project,
      developmentLinked: true,
    });
  });
});
