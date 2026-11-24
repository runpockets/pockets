import { afterEach, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { invoke } from "./api";
import PreviewNetworkSettings from "./PreviewNetworkSettings";

vi.mock("./api", () => ({ invoke: vi.fn() }));
const invokeMock = vi.mocked(invoke);

afterEach(() => {
  cleanup();
  invokeMock.mockReset();
});

it("shows saved hostnames separately and saves individual additions and removals", async () => {
  let saved = ["saved.example.com"];
  let rejectEdit = true;
  invokeMock.mockImplementation(async (command, args) => {
    if (command === "project_preview_network")
      return { hosts: saved, previewApis: [] };
    if (command === "set_project_preview_network") {
      if (rejectEdit) {
        rejectEdit = false;
        throw new Error("Stop the preview before changing its API access.");
      }
      saved = (args as { hosts: string[] }).hosts.map((host) =>
        host.toLowerCase(),
      );
      return saved;
    }
    throw new Error(`Unexpected command: ${command}`);
  });
  render(<PreviewNetworkSettings projectId="test-project" />);
  expect(invokeMock).not.toHaveBeenCalled();
  const details = screen.getByText("Preview API access").closest("details")!;
  details.open = true;
  fireEvent(details, new Event("toggle"));
  const field =
    await screen.findByLabelText<HTMLInputElement>("Add a hostname");
  const list = within(screen.getByRole("list", { name: "Allowed hostnames" }));
  const add = screen.getByRole<HTMLButtonElement>("button", {
    name: "Add hostname",
  });
  expect(list.getByText("saved.example.com")).toBeTruthy();
  expect(field.value).toBe("");
  expect(add.disabled).toBe(true);

  fireEvent.change(field, { target: { value: "SAVED.example.com" } });
  expect(add.disabled).toBe(true);
  fireEvent.change(field, { target: { value: " API.Example.com " } });
  fireEvent.click(add);
  expect((await screen.findByRole("alert")).textContent).toContain(
    "Stop the preview",
  );
  expect(field.value.trim()).toBe("API.Example.com");
  fireEvent.click(add);
  expect(await list.findByText("api.example.com")).toBeTruthy();
  expect(field.value).toBe("");
  expect(invokeMock).toHaveBeenCalledWith("set_project_preview_network", {
    projectId: "test-project",
    hosts: ["saved.example.com", "API.Example.com"],
  });

  fireEvent.click(
    list.getByRole("button", { name: "Remove saved.example.com" }),
  );
  await waitFor(() => expect(list.queryByText("saved.example.com")).toBeNull());
  expect(list.getByText("api.example.com")).toBeTruthy();
  expect(invokeMock).toHaveBeenCalledWith("set_project_preview_network", {
    projectId: "test-project",
    hosts: ["api.example.com"],
  });
});

it("renders matched previewApis name and purpose and marks unmatched hosts as added manually", async () => {
  invokeMock.mockImplementation(async (command) => {
    if (command === "project_preview_network") {
      return {
        hosts: ["api.livecoinwatch.com", "custom.example.com"],
        previewApis: [
          {
            host: "API.LiveCoinWatch.com",
            name: "Live Coin Watch",
            purpose: "Fetch live cryptocurrency prices.",
          },
        ],
      };
    }
    throw new Error(`Unexpected command: ${command}`);
  });

  render(<PreviewNetworkSettings projectId="test-project" />);
  const details = screen.getByText("Preview API access").closest("details")!;
  details.open = true;
  fireEvent(details, new Event("toggle"));

  const list = await screen.findByRole("list", { name: "Allowed hostnames" });
  const items = within(list).getAllByRole("listitem");
  expect(items).toHaveLength(2);

  const matched = within(items[0]);
  expect(matched.getByText("Live Coin Watch")).toBeTruthy();
  expect(matched.getByText("api.livecoinwatch.com")).toBeTruthy();
  expect(matched.getByText("Fetch live cryptocurrency prices.")).toBeTruthy();
  expect(matched.queryByText("Added manually")).toBeNull();

  const manual = within(items[1]);
  expect(manual.getByText("custom.example.com")).toBeTruthy();
  expect(manual.getByText("Added manually")).toBeTruthy();
});

it("falls back to previewApis prop when project_preview_network omits previewApis", async () => {
  invokeMock.mockImplementation(async (command) => {
    if (command === "project_preview_network") {
      return { hosts: ["api.kraken.com"] };
    }
    throw new Error(`Unexpected command: ${command}`);
  });

  render(
    <PreviewNetworkSettings
      projectId="test-project"
      previewApis={[
        {
          host: "api.kraken.com",
          name: "Kraken",
          purpose: "Read BTC ticker data.",
        },
      ]}
    />,
  );
  const details = screen.getByText("Preview API access").closest("details")!;
  details.open = true;
  fireEvent(details, new Event("toggle"));

  const list = await screen.findByRole("list", { name: "Allowed hostnames" });
  expect(within(list).getByText("Kraken")).toBeTruthy();
  expect(within(list).getByText("api.kraken.com")).toBeTruthy();
  expect(within(list).getByText("Read BTC ticker data.")).toBeTruthy();
});

it("shows the saved count in the header and an intro line once read", async () => {
  invokeMock.mockImplementation(async (command: string) => {
    if (command === "project_preview_network") {
      return { hosts: ["api.one.example", "api.two.example"] };
    }
    throw new Error(`Unexpected command: ${command}`);
  });
  render(<PreviewNetworkSettings projectId="count-project" />);
  const details = screen.getByText("Preview API access").closest("details")!;
  expect(details.querySelector(".preview-network-count")).toBeNull();
  details.open = true;
  fireEvent(details, new Event("toggle"));
  expect(await screen.findByText("2 allowed")).toBeTruthy();
  expect(screen.getByText(/read-only HTTPS requests/)).toBeTruthy();
  // The remove control is an icon button that still names its host.
  expect(
    screen.getByRole("button", { name: "Remove api.two.example" }).textContent,
  ).toBe("");
});
