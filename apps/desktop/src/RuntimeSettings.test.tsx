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

function openSection() {
  const details = screen.getByText("Runtime variables").closest("details")!;
  details.open = true;
  fireEvent(details, new Event("toggle"));
  return details;
}

it("keeps secrets write-only and loads each environment on demand", async () => {
  invokeMock.mockImplementation(async (command, args) => {
    if (command === "project_runtime_settings") {
      return args && "environment" in args && args.environment === "preview"
        ? { variables: [{ name: "PRICING_API_KEY", kind: "secret" }] }
        : {
            variables: [{ name: "CURRENCY", kind: "plain", value: "NZD" }],
          };
    }
    if (command === "change_project_runtime_settings")
      return { variables: [{ name: "PRICING_API_KEY", kind: "secret" }] };
    throw new Error(`Unexpected command ${command}`);
  });

  render(<RuntimeSettings projectId="sample-project" disabled={false} />);
  expect(invokeMock).not.toHaveBeenCalled();
  openSection();

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

it("renders declared variables with purpose/type and prefills detected source variables", async () => {
  const declared = [
    {
      name: "PRICING_API_KEY",
      kind: "secret" as const,
      required: true,
      purpose: "Authenticates market data lookups.",
    },
    {
      name: "CURRENCY",
      kind: "plain" as const,
      required: false,
      purpose: "Default display currency.",
    },
  ];
  let saved: Array<
    | { name: string; kind: "plain"; value: string }
    | { name: string; kind: "secret" }
  > = [];

  invokeMock.mockImplementation(async (command, args) => {
    if (command === "project_runtime_settings") {
      return {
        variables: saved,
        declared,
        detected: ["EXTRA_FLAG"],
      };
    }
    if (command === "change_project_runtime_settings") {
      const change = (
        args as {
          change:
            | {
                action: "set";
                name: string;
                kind: "plain" | "secret";
                value: string;
              }
            | { action: "remove"; name: string };
        }
      ).change;
      if (change.action === "set") {
        saved = [
          ...saved.filter((item) => item.name !== change.name),
          change.kind === "plain"
            ? { name: change.name, kind: "plain", value: change.value }
            : { name: change.name, kind: "secret" },
        ];
      } else {
        saved = saved.filter((item) => item.name !== change.name);
      }
      return {
        variables: saved,
        declared,
        detected: ["EXTRA_FLAG"],
      };
    }
    throw new Error(`Unexpected command ${command}`);
  });

  render(<RuntimeSettings projectId="sample-project" disabled={false} />);
  openSection();

  expect(
    await screen.findByText("Authenticates market data lookups."),
  ).toBeTruthy();
  expect(screen.getByText("1 required")).toBeTruthy();
  expect(screen.getByText("0 saved")).toBeTruthy();

  // Clicking Set on an optional plain declared variable locks its name and
  // selects Plain text automatically.
  fireEvent.click(screen.getByRole("button", { name: "Set CURRENCY" }));
  const nameInput = screen.getByLabelText<HTMLInputElement>("Variable name");
  expect(nameInput.value).toBe("CURRENCY");
  expect(nameInput.readOnly).toBe(true);
  const plainInput = screen.getByLabelText<HTMLInputElement>("Value");
  expect(plainInput.type).toBe("text");
  fireEvent.change(plainInput, { target: { value: "NZD" } });
  fireEvent.click(screen.getByRole("button", { name: "Save variable" }));

  expect(await screen.findByText("NZD")).toBeTruthy();
  expect(invokeMock).toHaveBeenCalledWith("change_project_runtime_settings", {
    id: "sample-project",
    environment: "preview",
    change: {
      action: "set",
      name: "CURRENCY",
      kind: "plain",
      value: "NZD",
      replace: false,
    },
  });

  // Clicking a detected variable prefills the editable Variable name field and
  // removes it from suggestions once saved.
  fireEvent.click(
    screen.getByRole("button", { name: "Use detected variable EXTRA_FLAG" }),
  );
  const prefilled = screen.getByLabelText<HTMLInputElement>("Variable name");
  expect(prefilled.value).toBe("EXTRA_FLAG");
  expect(prefilled.readOnly).toBe(false);
  const secretInput = screen.getByLabelText<HTMLInputElement>("Secret value");
  fireEvent.change(secretInput, { target: { value: "flag-secret" } });
  fireEvent.click(screen.getByRole("button", { name: "Add variable" }));

  expect(
    await screen.findByRole("button", { name: "Replace EXTRA_FLAG" }),
  ).toBeTruthy();
  expect(
    screen.queryByRole("button", { name: "Use detected variable EXTRA_FLAG" }),
  ).toBeNull();
  expect(screen.getByText("Added manually")).toBeTruthy();
});
