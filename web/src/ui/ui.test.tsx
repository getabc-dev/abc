// @vitest-environment jsdom

// abc's components: what each does, and the name assistive technology gives it.

import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, describe, expect, test, vi } from "vitest";

import { AppShell, initials, type MachinePill } from "./AppShell";
import { Button } from "./Button";
import { Card } from "./Card";
import { MachinesIcon } from "./icons";
import { StatusPill } from "./StatusPill";
import { TextField } from "./TextField";

afterEach(cleanup);

describe("Button", () => {
  test("is a button named by its label that reports presses, by pointer and keyboard", async () => {
    const pressed = vi.fn<() => void>();
    render(<Button onPress={pressed}>Claim this server</Button>);
    const button = screen.getByRole("button", { name: "Claim this server" });
    await userEvent.click(button);
    await userEvent.keyboard("{Enter}");
    expect(pressed).toHaveBeenCalledTimes(2);
    expect(button.className).toContain("border-rule-strong");
  });

  test("shows keyboard focus, and the primary variant is filled with the accent", async () => {
    render(<Button variant="primary">Go</Button>);
    await userEvent.tab();
    const button = screen.getByRole("button", { name: "Go" });
    expect(button).toBe(document.activeElement);
    expect(button.hasAttribute("data-focus-visible")).toBe(true);
    expect(button.className).toContain("bg-accent");
  });

  test("while pending it is announced as such and ignores presses", async () => {
    const pressed = vi.fn<() => void>();
    render(
      <Button isPending onPress={pressed}>
        Claiming
      </Button>,
    );
    const button = screen.getByRole("button", { name: "Claiming" });
    await userEvent.click(button);
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
    await userEvent.type(input, "ABCD");
    expect(input).toHaveProperty("value", "ABCD");
    expect(input.getAttribute("aria-invalid")).toBeNull();
    expect(input.className).toContain("font-mono");
  });

  test("an error marks it invalid and is read with it", () => {
    render(<Controlled error="That is not the server's code." />);
    const input = screen.getByRole("textbox", { name: "Setup code" });
    expect(input.getAttribute("aria-invalid")).toBe("true");
    const described = (input.getAttribute("aria-describedby") ?? "").split(" ");
    const texts = described.map((id) => document.getElementById(id)?.textContent);
    expect(texts).toContain("That is not the server's code.");
  });
});

describe("StatusPill", () => {
  test("always says its state in words, beside a dot hidden from assistive technology", () => {
    const { container } = render(
      <>
        <StatusPill tone="success" label="Online" />
        <StatusPill tone="neutral" label="Offline" />
        <StatusPill tone="warning" label="Reconnecting" />
        <StatusPill tone="danger" label="Revoked" />
      </>,
    );
    expect(container.textContent).toBe("OnlineOfflineReconnectingRevoked");
    const dots = container.querySelectorAll('[aria-hidden="true"]');
    expect(dots).toHaveLength(4);
    expect(dots[0]?.className).toContain("bg-success");
    expect(dots[1]?.className).toContain("border-ink-muted");
    expect(screen.getByText("Reconnecting").className).toContain("text-warning-ink");
  });
});

const online = { tone: "success", label: "Online" } as const;

describe("Card", () => {
  test("is a region named by its heading, at the level asked for", () => {
    render(
      <Card title="lab box" level={3} status={online}>
        <p>Linux on x86_64</p>
      </Card>,
    );
    const region = screen.getByRole("region", { name: "lab box" });
    expect(within(region).getByRole("heading", { level: 3, name: "lab box" })).toBeDefined();
    expect(within(region).getByText("Online")).toBeDefined();
    expect(within(region).getByText("Linux on x86_64")).toBeDefined();
  });
});

const navigation = [
  { href: "/machines", label: "Machines", icon: <MachinesIcon />, current: true },
  { href: "/later", label: "Later", icon: null, current: false },
];

function shell(machines: readonly MachinePill[] | null) {
  return render(
    <AppShell
      workspace="Home lab"
      person="Ada Lovelace"
      machines={machines}
      navigation={navigation}
    >
      <h1>Page</h1>
    </AppShell>,
  );
}

describe("AppShell", () => {
  test("has one main navigation, marking the current page, around the page", () => {
    shell([]);
    const nav = screen.getByRole("navigation", { name: "Main" });
    const current = within(nav).getByRole("link", { name: "Machines" });
    expect(current.getAttribute("href")).toBe("/machines");
    expect(current.getAttribute("aria-current")).toBe("page");
    expect(
      within(nav).getByRole("link", { name: "Later" }).getAttribute("aria-current"),
    ).toBeNull();
    expect(screen.getAllByRole("navigation")).toHaveLength(1);
    expect(within(screen.getByRole("main")).getByRole("heading", { name: "Page" })).toBeDefined();
    expect(within(nav).getByText("abc")).toBeDefined();
  });

  test("names the workspace and the person, whose initials it shows", () => {
    shell([]);
    const banner = screen.getByRole("banner");
    expect(within(banner).getByText("Home lab")).toBeDefined();
    expect(within(banner).getByText("Signed in as Ada Lovelace")).toBeDefined();
    expect(within(banner).getByText("AL").getAttribute("aria-hidden")).toBe("true");
  });

  test("lists each machine with its state in words, and counts those online", () => {
    shell([
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

  test("counts the machines past the fourth, and says when there are none", () => {
    const many = ["a", "b", "c", "d", "e", "f"].map((name) => ({ id: name, name, online: true }));
    const { unmount } = shell(many);
    const items = within(screen.getByRole("list", { name: "Machines" })).getAllByRole("listitem");
    expect(items).toHaveLength(5);
    expect(items[4]?.textContent).toBe("+2 more");
    unmount();
    shell([]);
    expect(screen.getByText("No machines")).toBeDefined();
  });

  test("shows no machines while they load", () => {
    shell(null);
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
