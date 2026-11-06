// @vitest-environment jsdom

// abc's components, and the shadcn/ui pieces under them: what each does, and the name assistive
// technology gives it. shadcn's accessibility rests on Base UI, so these tests are what hold it.

import {
  createMemoryHistory,
  createRootRoute,
  createRouter,
  RouterProvider,
} from "@tanstack/react-router";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MonitorSmartphoneIcon } from "lucide-react";
import { useState } from "react";
import { afterEach, beforeAll, describe, expect, test, vi } from "vitest";

import { Button } from "@/components/ui/button";
import { stubMatchMedia } from "@/testing";

import { AppShell, initials, type MachinePill } from "./AppShell";
import { CopyableText } from "./CopyableText";
import { Status } from "./Status";
import { TextField } from "./TextField";
import { ThemeMenu } from "./ThemeMenu";
import { TitledCard } from "./TitledCard";

beforeAll(async () => {
  stubMatchMedia(false);
  // The theme script the page loads before the app; the theme menu talks to it.
  await import("../../public/theme.js");
});

afterEach(cleanup);

describe("Button", () => {
  test("is a button named by its label that reports presses, by pointer and keyboard", async () => {
    const pressed = vi.fn<() => void>();
    render(<Button onClick={pressed}>Claim this server</Button>);
    const button = screen.getByRole("button", { name: "Claim this server" });
    await userEvent.click(button);
    button.focus();
    await userEvent.keyboard("{Enter}");
    await userEvent.keyboard(" ");
    expect(pressed).toHaveBeenCalledTimes(3);
  });

  test("takes keyboard focus in order, with the focus ring in the ring token", async () => {
    render(
      <>
        <Button>First</Button>
        <Button variant="outline">Second</Button>
      </>,
    );
    await userEvent.tab();
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "First" }));
    await userEvent.tab();
    const second = screen.getByRole("button", { name: "Second" });
    expect(document.activeElement).toBe(second);
    expect(second.className).toContain("focus-visible:ring-ring");
    expect(second.className).toContain("focus-visible:ring-3");
  });

  test("while busy it stays focusable, says it is disabled and ignores presses", async () => {
    const pressed = vi.fn<() => void>();
    render(
      <Button disabled focusableWhenDisabled onClick={pressed}>
        Claiming…
      </Button>,
    );
    const button = screen.getByRole("button", { name: "Claiming…" });
    await userEvent.click(button);
    await userEvent.tab();
    expect(document.activeElement).toBe(button);
    await userEvent.keyboard("{Enter}");
    expect(pressed).not.toHaveBeenCalled();
    expect(button.getAttribute("aria-disabled")).toBe("true");
  });
});

function Controlled({ error }: { readonly error?: string }) {
  const [value, setValue] = useState("");
  return (
    <TextField
      label="Setup code"
      description="Printed by abc server setup-code."
      value={value}
      onChange={setValue}
      error={error}
      mono
    />
  );
}

describe("TextField", () => {
  test("is a text box named by its label and described by its description", async () => {
    render(<Controlled />);
    const input = screen.getByRole("textbox", { name: "Setup code" });
    const described = input.getAttribute("aria-describedby") ?? "";
    expect(document.getElementById(described)?.textContent).toBe(
      "Printed by abc server setup-code.",
    );
    await userEvent.click(screen.getByText("Setup code"));
    expect(document.activeElement).toBe(input);
    await userEvent.keyboard("ABCD");
    expect(input).toHaveProperty("value", "ABCD");
    expect(input.getAttribute("aria-invalid")).toBe("false");
    expect(input.className).toContain("font-mono");
  });

  test("an error marks it invalid and is read with it", () => {
    render(<Controlled error="That is not the server's code." />);
    const input = screen.getByRole("textbox", { name: "Setup code" });
    expect(input.getAttribute("aria-invalid")).toBe("true");
    const described = (input.getAttribute("aria-describedby") ?? "").split(" ");
    const texts = described.map((id) => document.getElementById(id)?.textContent);
    expect(texts).toEqual(["Printed by abc server setup-code.", "That is not the server's code."]);
  });
});

describe("Status", () => {
  test("always says its state in words, beside a dot hidden from assistive technology", () => {
    const { container } = render(
      <>
        <Status tone="success" label="Online" />
        <Status tone="neutral" label="Offline" />
        <Status tone="warning" label="Reconnecting" />
        <Status tone="danger" label="Revoked" />
      </>,
    );
    expect(container.textContent).toBe("OnlineOfflineReconnectingRevoked");
    const dots = container.querySelectorAll('[aria-hidden="true"]');
    expect(dots).toHaveLength(4);
    expect(dots[0]?.className).toContain("bg-success");
    expect(dots[1]?.className).toContain("border-muted-foreground");
    expect(screen.getByText("Reconnecting").className).toContain("text-warning-foreground");
  });
});

const online = { tone: "success", label: "Online" } as const;

describe("TitledCard", () => {
  test("is a region named by its heading, at the level asked for", () => {
    render(
      <TitledCard title="lab box" level={3} status={online}>
        <p>Linux on x86_64</p>
      </TitledCard>,
    );
    const region = screen.getByRole("region", { name: "lab box" });
    expect(within(region).getByRole("heading", { level: 3, name: "lab box" })).toBeDefined();
    expect(within(region).getByText("Online")).toBeDefined();
    expect(within(region).getByText("Linux on x86_64")).toBeDefined();
  });
});

describe("ThemeMenu", () => {
  test("names the theme in force, and choosing one by keyboard applies and keeps it", async () => {
    render(<ThemeMenu />);
    const trigger = screen.getByRole("button", { name: "Theme: System" });
    trigger.focus();
    await userEvent.keyboard("{Enter}");
    const menu = await screen.findByRole("menu");
    const choices = within(menu).getAllByRole("menuitemradio");
    expect(choices.map((choice) => choice.textContent)).toEqual(["Light", "Dark", "System"]);
    expect(choices.map((choice) => choice.getAttribute("aria-checked"))).toEqual([
      "false",
      "false",
      "true",
    ]);
    await userEvent.click(within(menu).getByRole("menuitemradio", { name: "Dark" }));
    expect(document.documentElement.classList.contains("dark")).toBe(true);
    expect(window.localStorage.getItem("abc-theme")).toBe("dark");
    expect(await screen.findByRole("button", { name: "Theme: Dark" })).toBeDefined();
    await waitFor(() => {
      expect(screen.queryByRole("menu")).toBeNull();
    });
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Theme: Dark" }));

    await userEvent.keyboard("{Enter}");
    // The menu highlights its first item once it has opened; a key pressed before that is lost,
    // and {End}{Enter} sent at once chose Light about one run in two. Each key waits for the
    // highlight it moves.
    const reopened = await screen.findByRole("menu");
    const [light, , system] = within(reopened).getAllByRole("menuitemradio");
    await waitFor(() => {
      expect(document.activeElement).toBe(light);
    });
    await userEvent.keyboard("{End}");
    await waitFor(() => {
      expect(document.activeElement).toBe(system);
    });
    await userEvent.keyboard("{Enter}");
    expect(document.documentElement.classList.contains("dark")).toBe(false);
    expect(window.localStorage.getItem("abc-theme")).toBeNull();
    expect(await screen.findByRole("button", { name: "Theme: System" })).toBeDefined();
  });
});

const navigation = [
  {
    to: "/machines",
    label: "Machines",
    icon: <MonitorSmartphoneIcon aria-hidden="true" />,
    current: true,
  },
] as const;

/** The shell, as the router shows it at `/machines`. */
async function shell(machines: readonly MachinePill[] | null) {
  const router = createRouter({
    routeTree: createRootRoute({
      component: () => (
        <AppShell
          workspace="Home lab"
          person="Ada Lovelace"
          machines={machines}
          navigation={navigation}
          notice={null}
        >
          <h1>Page</h1>
        </AppShell>
      ),
    }),
    history: createMemoryHistory({ initialEntries: ["/machines"] }),
  });
  const view = render(<RouterProvider router={router} />);
  await screen.findByRole("heading", { name: "Page" });
  return view;
}

describe("AppShell", () => {
  test("has one main navigation, marking the current page, around the page", async () => {
    await shell([]);
    const nav = screen.getByRole("navigation", { name: "Main" });
    const current = within(nav).getByRole("link", { name: "Machines" });
    expect(current.getAttribute("href")).toBe("/machines");
    expect(current.getAttribute("aria-current")).toBe("page");
    expect(screen.getAllByRole("navigation")).toHaveLength(1);
    expect(within(screen.getByRole("main")).getByRole("heading", { name: "Page" })).toBeDefined();
    expect(within(nav).getByText("abc")).toBeDefined();
  });

  test("names the workspace, the theme and the person, whose initials it shows", async () => {
    await shell([]);
    const banner = screen.getByRole("banner");
    expect(within(banner).getByText("Home lab")).toBeDefined();
    expect(within(banner).getByRole("button", { name: /^Theme: / })).toBeDefined();
    expect(within(banner).getByText("Signed in as Ada Lovelace")).toBeDefined();
    expect(within(banner).getByText("AL").getAttribute("aria-hidden")).toBe("true");
  });

  test("lists each machine with its state in words, and counts those online", async () => {
    await shell([
      { id: "1", name: "lab box", online: true },
      { id: "2", name: "laptop", online: false },
    ]);
    const list = screen.getByRole("list", { name: "Machines" });
    expect(
      within(list)
        .getAllByRole("listitem")
        .map((item) => item.textContent),
    ).toEqual(["lab box, online", "laptop, offline"]);
    expect(screen.getByText("1 of 2 online")).toBeDefined();
  });

  test("counts the machines past the fourth, and says when there are none", async () => {
    const many = ["a", "b", "c", "d", "e", "f"].map((name) => ({ id: name, name, online: true }));
    const { unmount } = await shell(many);
    const items = within(screen.getByRole("list", { name: "Machines" })).getAllByRole("listitem");
    expect(items).toHaveLength(5);
    expect(items[4]?.textContent).toBe("+2 more");
    unmount();
    await shell([]);
    expect(screen.getByText("No machines")).toBeDefined();
  });

  test("shows no machines while they load", async () => {
    await shell(null);
    expect(screen.queryByRole("list", { name: "Machines" })).toBeNull();
    expect(screen.queryByText("No machines")).toBeNull();
  });

  test("initials are the first letters of the first two words", () => {
    expect(initials("ada")).toBe("A");
    expect(initials("  Ada  King Lovelace ")).toBe("AK");
    expect(initials("élise ñ")).toBe("ÉÑ");
    expect(initials("")).toBe("");
  });
});

describe("CopyableText", () => {
  test("shows the whole value and copies exactly it, saying Copied to screen readers", async () => {
    const user = userEvent.setup();
    const value = "abc_enr_7hQ2xL9vKp3mN8rT4wY6zB1cD5fG0jH2kL4nP6qR8sT0vX2yZ4";
    render(<CopyableText value={value} label="the token" />);
    expect(screen.getByText(value).tagName).toBe("CODE");
    const button = screen.getByRole("button", { name: "Copy the token" });
    expect(button.textContent).toBe("Copy");
    await user.click(button);
    await waitFor(() => {
      expect(button.textContent).toBe("Copied");
    });
    expect(await navigator.clipboard.readText()).toBe(value);
    expect(screen.getByRole("status").textContent).toBe("Copied the token");
  });

  test("says Copied only for a moment", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    try {
      const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime.bind(vi) });
      render(<CopyableText value="abc worker" label="the worker command" />);
      const button = screen.getByRole("button", { name: "Copy the worker command" });
      await user.click(button);
      await waitFor(() => {
        expect(button.textContent).toBe("Copied");
      });
      vi.advanceTimersByTime(1999);
      expect(button.textContent).toBe("Copied");
      vi.advanceTimersByTime(1);
      await waitFor(() => {
        expect(button.textContent).toBe("Copy");
      });
      expect(screen.getByRole("status").textContent).toBe("");
    } finally {
      vi.useRealTimers();
    }
  });

  test("selects the value when the clipboard is refused", async () => {
    const user = userEvent.setup();
    const refused = vi
      .spyOn(navigator.clipboard, "writeText")
      .mockRejectedValue(new DOMException("refused", "NotAllowedError"));
    try {
      render(
        <CopyableText value="abc worker enrol https://abc.example.ts.net" label="the command" />,
      );
      await user.click(screen.getByRole("button", { name: "Copy the command" }));
      await waitFor(() => {
        expect(window.getSelection()?.toString()).toBe(
          "abc worker enrol https://abc.example.ts.net",
        );
      });
      expect(screen.getByRole("status").textContent).toBe(
        "Selected the command; copy it with your keyboard",
      );
      expect(screen.getByRole("button", { name: "Copy the command" }).textContent).toBe("Copy");
    } finally {
      refused.mockRestore();
    }
  });
});
