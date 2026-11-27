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
import NewProject, {
  type GitHubPocketPreview,
  type HubCatalogResponse,
} from "./NewProject";

vi.mock("./api", () => ({ invoke: vi.fn() }));
const native = vi.mocked(invoke);

const project = {
  id: "created-project",
  manifest: { metadata: { name: "My portfolio" } },
};

const hubCatalog: HubCatalogResponse = {
  source: "remote",
  hubUrl:
    "https://raw.githubusercontent.com/runpockets/pockets-hub/main/catalog.json",
  pockets: [
    {
      repo: "runpockets/coinkiwi",
      repoUrl: "https://github.com/runpockets/coinkiwi",
      id: "coinkiwi",
      name: "CoinKiwi",
      description: "Track crypto portfolios and NZD prices locally.",
      version: "0.4.0",
      category: "Finance",
      verified: true,
      metadataSource: "repo",
      hostSupported: true,
      architectures: ["arm64", "amd64"],
      previewApis: [
        {
          host: "api.coingecko.com",
          name: "CoinGecko",
          purpose: "Fetch live market prices",
        },
      ],
      runtimeVariables: [],
    },
  ],
};

const validPreview: GitHubPocketPreview = {
  owner: "runpockets",
  repo: "coinkiwi",
  slug: "runpockets/coinkiwi",
  repoUrl: "https://github.com/runpockets/coinkiwi",
  manifest: {
    metadata: {
      id: "coinkiwi",
      name: "CoinKiwi",
      version: "0.4.0",
      description: "Track crypto portfolios and NZD prices locally.",
    },
    runtime: {
      image: "localhost/pockets-coinkiwi:0.4.0",
      architectures: ["arm64", "amd64"],
    },
    web: { port: 8080, healthcheck: "/health" },
    storage: { name: "coinkiwi-data", path: "/data" },
    previewApis: [
      {
        host: "api.coingecko.com",
        name: "CoinGecko",
        purpose: "Fetch live market prices",
      },
    ],
    runtimeVariables: [
      {
        name: "COINGECKO_API_KEY",
        kind: "secret",
        purpose: "Optional API key",
        required: false,
      },
    ],
  },
  hostSupported: true,
};

function stubInvoke(
  handlers: Record<string, (args?: Record<string, unknown>) => unknown>,
) {
  native.mockImplementation(async (command, args) => {
    if (command in handlers) {
      return handlers[command](args as Record<string, unknown> | undefined);
    }
    if (command === "list_hub_pockets") {
      return hubCatalog;
    }
    return null;
  });
}

function renderForm() {
  const onCreated = vi.fn();
  const onSelectProject = vi.fn();
  const view = render(
    <NewProject
      sidebarCollapsed={false}
      previewCollapsed={false}
      onToggleSidebar={vi.fn()}
      onTogglePreview={vi.fn()}
      onCreated={onCreated}
      onSelectProject={onSelectProject}
    />,
  );
  return { ...view, onCreated, onSelectProject };
}

describe("New project", () => {
  beforeEach(() => {
    native.mockReset();
    stubInvoke({});
  });
  afterEach(cleanup);

  it("creates a linked starter project without a project brief field and auto-derives the folder slug", async () => {
    stubInvoke({ create_development_project: () => project });
    const { onCreated } = renderForm();
    expect(
      screen.queryByLabelText(/brief|what do you want to build/i),
    ).toBeNull();

    fireEvent.change(screen.getByLabelText("Project name"), {
      target: { value: "My portfolio" },
    });
    expect(screen.getByText("my-portfolio")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Create project…" }));
    await waitFor(() =>
      expect(onCreated).toHaveBeenCalledWith({
        ...project,
        developmentLinked: true,
      }),
    );
    expect(native).toHaveBeenCalledWith("create_development_project", {
      name: "My portfolio",
      folderName: "my-portfolio",
    });
  });

  it("preserves form input on picker cancellation and respects an edited folder name", async () => {
    stubInvoke({ create_development_project: () => null });
    const { onCreated } = renderForm();
    fireEvent.change(screen.getByLabelText("Project name"), {
      target: { value: "First name" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Change" }));
    fireEvent.change(screen.getByLabelText("Folder name"), {
      target: { value: "custom-folder" },
    });
    fireEvent.change(screen.getByLabelText("Project name"), {
      target: { value: "Second name" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Create project…" }));
    await waitFor(() =>
      expect(
        screen
          .getByRole("button", { name: "Create project…" })
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
    stubInvoke({
      create_development_project: () =>
        new Promise((_resolve, fail) => {
          reject = fail;
        }),
    });
    renderForm();
    fireEvent.change(screen.getByLabelText("Project name"), {
      target: { value: "Tracker" },
    });
    const form = screen.getByLabelText("Project name").closest("form")!;
    fireEvent.submit(form);
    fireEvent.submit(form);
    const createCalls = native.mock.calls.filter(
      ([cmd]) => cmd === "create_development_project",
    );
    expect(createCalls).toHaveLength(1);
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

  it("links existing source from the Folder tab without requiring creation fields, and reports registration after unmount", async () => {
    let resolve!: (value: unknown) => void;
    stubInvoke({
      link_development_folder: () =>
        new Promise((done) => {
          resolve = done;
        }),
    });
    const { onCreated, unmount } = renderForm();
    fireEvent.click(screen.getByRole("tab", { name: "Local folder" }));
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
    fireEvent.click(screen.getByRole("button", { name: "Change" }));
    const folderInput = screen.getByLabelText<HTMLInputElement>("Folder name");
    fireEvent.change(folderInput, { target: { value: "Bad Folder!" } });
    expect(folderInput.getAttribute("aria-invalid")).toBe("true");
    expect(
      screen.getByText(/Use lowercase letters, numbers, and single hyphens/),
    ).toBeTruthy();
    expect(
      screen
        .getByRole("button", { name: "Create project…" })
        .hasAttribute("disabled"),
    ).toBe(true);
  });

  it("lists CoinKiwi from Pockets Hub on the default tab with its repo description and pulls it on click", async () => {
    const coinKiwiProject = {
      id: "coinkiwi-id",
      manifest: { metadata: { name: "CoinKiwi" } },
    };
    stubInvoke({ add_github_pocket: () => coinKiwiProject });
    const { onCreated } = renderForm();

    const hubPanel = screen.getByRole("tabpanel", { name: "Pockets Hub" });
    expect(await within(hubPanel).findByText("CoinKiwi")).toBeTruthy();
    expect(
      within(hubPanel).getByText(
        "Track crypto portfolios and NZD prices locally.",
      ),
    ).toBeTruthy();
    const repoLink = within(hubPanel).getByRole("link", {
      name: "runpockets/coinkiwi",
    });
    expect(repoLink.getAttribute("href")).toBe(
      "https://github.com/runpockets/coinkiwi",
    );
    expect(repoLink.getAttribute("target")).toBe("_blank");
    expect(repoLink.getAttribute("rel")).toBe("noopener noreferrer");
    expect(within(hubPanel).queryByText(/web service|setting/)).toBeNull();
    expect(within(hubPanel).getByText(/with Podman/)).toBeTruthy();

    await act(async () => {
      fireEvent.click(
        within(hubPanel).getByRole("button", { name: "Add to Pockets" }),
      );
    });
    expect(native).toHaveBeenCalledWith("add_github_pocket", {
      source: "runpockets/coinkiwi",
    });
    expect(onCreated).toHaveBeenCalledWith({
      ...coinKiwiProject,
      developmentLinked: true,
    });
  });

  it("shows In your Pockets and Manage when a Hub Pocket is already added", async () => {
    stubInvoke({
      list_hub_pockets: () => ({
        ...hubCatalog,
        pockets: [
          { ...hubCatalog.pockets[0], existingProjectId: "existing-coinkiwi" },
        ],
      }),
    });
    const { onSelectProject } = renderForm();

    const hubPanel = screen.getByRole("tabpanel", { name: "Pockets Hub" });
    expect(await within(hubPanel).findByText("In your Pockets")).toBeTruthy();
    const favicon = hubPanel.querySelector("img.new-project-hub-favicon");
    expect(favicon?.getAttribute("src")).toBe(
      "/api/projects/existing-coinkiwi/favicon",
    );
    const manageBtn = within(hubPanel).getByRole("button", {
      name: "Manage",
    });
    fireEvent.click(manageBtn);
    expect(onSelectProject).toHaveBeenCalledWith("existing-coinkiwi");
  });

  it("shows a Hub Pocket's site favicon once loaded and renders nothing when missing", async () => {
    renderForm();

    const hubPanel = screen.getByRole("tabpanel", { name: "Pockets Hub" });
    await within(hubPanel).findByText("CoinKiwi");

    const img = hubPanel.querySelector("img.new-project-hub-favicon")!;
    expect(img).toBeTruthy();
    expect(img.getAttribute("src")).toBe(
      "/api/hub/favicon?repo=runpockets%2Fcoinkiwi",
    );
    expect(img.getAttribute("data-loaded")).toBeNull();

    fireEvent.load(img);
    expect(img.getAttribute("data-loaded")).toBe("true");

    fireEvent.error(img);
    expect(hubPanel.querySelector("img.new-project-hub-favicon")).toBeNull();
  });

  it("validates a GitHub repository on the GitHub tab before pulling and shows its manifest preview", async () => {
    stubInvoke({
      inspect_github_pocket: () => validPreview,
      add_github_pocket: () => project,
    });
    const { onCreated } = renderForm();

    fireEvent.click(screen.getByRole("tab", { name: "GitHub" }));
    fireEvent.change(screen.getByLabelText("GitHub repository"), {
      target: { value: "https://github.com/runpockets/coinkiwi" },
    });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Check repo" }));
    });
    expect(native).toHaveBeenCalledWith("inspect_github_pocket", {
      source: "https://github.com/runpockets/coinkiwi",
    });

    const preview = await screen.findByRole("region", {
      name: "Validated repository preview",
    });
    expect(within(preview).getByText("Valid Pocket repository")).toBeTruthy();
    expect(
      within(preview)
        .getByRole("link", { name: "runpockets/coinkiwi" })
        .getAttribute("href"),
    ).toBe("https://github.com/runpockets/coinkiwi");
    expect(within(preview).getByText("CoinKiwi")).toBeTruthy();
    expect(
      within(preview).getByText(
        "Track crypto portfolios and NZD prices locally.",
      ),
    ).toBeTruthy();

    await act(async () => {
      fireEvent.click(
        within(preview).getByRole("button", { name: "Pull Pocket" }),
      );
    });
    expect(native).toHaveBeenCalledWith("add_github_pocket", {
      source: "runpockets/coinkiwi",
    });
    expect(onCreated).toHaveBeenCalledWith({
      ...project,
      developmentLinked: true,
    });
  });

  it("shows a validation error for an invalid GitHub repo and clears it on edit", async () => {
    stubInvoke({
      inspect_github_pocket: () =>
        Promise.reject(
          new Error(
            "octocat/hello-world is not a valid Pocket repository: missing pocket.yaml at the repository root.",
          ),
        ),
    });
    renderForm();

    fireEvent.click(screen.getByRole("tab", { name: "GitHub" }));
    const input = screen.getByLabelText("GitHub repository");
    fireEvent.change(input, { target: { value: "octocat/hello-world" } });
    await act(async () => {
      fireEvent.click(screen.getByRole("button", { name: "Check repo" }));
    });
    expect((await screen.findByRole("alert")).textContent).toContain(
      "missing pocket.yaml",
    );

    fireEvent.change(input, { target: { value: "runpockets/coinkiwi" } });
    expect(screen.queryByRole("alert")).toBeNull();
  });
});
