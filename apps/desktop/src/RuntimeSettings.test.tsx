import { afterEach, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  within,
} from "@testing-library/react";
import { invoke } from "./api";
import RuntimeSettings from "./RuntimeSettings";

vi.mock("./api", () => ({ invoke: vi.fn() }));
const invokeMock = vi.mocked(invoke);

afterEach(() => {
  cleanup();
  invokeMock.mockReset();
});

it("keeps secrets write-only and loads each environment on demand", async () => {
  invokeMock.mockImplementation(async (command, args) => {
    if (command === "project_runtime_settings") {
      return args && "environment" in args && args.environment === "preview"
        ? [{ name: "PRICING_API_KEY", kind: "secret" }]
        : [{ name: "CURRENCY", kind: "plain", value: "NZD" }];
    }
    if (command === "change_project_runtime_settings")
      return [{ name: "PRICING_API_KEY", kind: "secret" }];
    throw new Error(`Unexpected command ${command}`);
  });

  render(<RuntimeSettings projectId="sample-project" disabled={false} />);
  expect(invokeMock).not.toHaveBeenCalled();
  const details = screen.getByText("Runtime variables").closest("details")!;
  details.open = true;
  fireEvent(details, new Event("toggle"));

  fireEvent.click(
    await screen.findByRole("button", { name: "Replace PRICING_API_KEY" }),
  );
  const secret = screen.getByLabelText<HTMLInputElement>("Secret value");
  expect(secret.type).toBe("password");
  expect(secret.value).toBe("");
  fireEvent.change(secret, { target: { value: "replacement-fixture" } });
  fireEvent.click(screen.getByRole("button", { name: "Save replacement" }));
  expect(secret.value).toBe("");
  expect(await screen.findByText("Variable saved.")).toBeTruthy();
  expect(invokeMock).toHaveBeenCalledWith("change_project_runtime_settings", {
    id: "sample-project",
    environment: "preview",
    change: {
      action: "set",
      name: "PRICING_API_KEY",
      kind: "secret",
      value: "replacement-fixture",
      replace: true,
    },
  });

  const envTabs = screen.getByRole("tablist", { name: "Environment" });
  fireEvent.click(within(envTabs).getByRole("tab", { name: "Installed app" }));
  expect(screen.queryByText("PRICING_API_KEY")).toBeNull();
  // Plain-text variables display their value directly in the list while secrets
  // remain write-only.
  expect(await screen.findByText("NZD")).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "Replace CURRENCY" }));
  expect(screen.getByLabelText<HTMLInputElement>("Value").value).toBe("NZD");
  expect(invokeMock).toHaveBeenCalledWith("project_runtime_settings", {
    id: "sample-project",
    environment: "installed",
  });
});
