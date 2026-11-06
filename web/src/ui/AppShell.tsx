// The frame of every signed-in page, after the canvas's Home A. On the desktop: the rail (the logo
// and the navigation) and the header (the workspace, a pill per machine and the person). On the
// phone, after the canvas's phone Home: a top bar (the workspace, the machines online and the
// person), the page, and the same navigation as a tab bar at the bottom. One `nav` is both the
// rail and the tab bar, so there is one landmark and one set of links whatever the width.

import type { ReactNode } from "react";
import { Link } from "react-aria-components";

import { Logo } from "./Logo";

export type NavItem = {
  readonly href: string;
  readonly label: string;
  readonly icon: ReactNode;
  readonly current: boolean;
};

export type MachinePill = {
  readonly id: string;
  readonly name: string;
  readonly online: boolean;
};

/** How many machines the header shows by name before it counts the rest. */
const PILLS_SHOWN = 4;

const graphemes = new Intl.Segmenter(undefined, { granularity: "grapheme" });

/** The initials of `name`: the first letters, as people see letters, of its first two words. */
export function initials(name: string): string {
  const words = name
    .trim()
    .split(/\s+/u)
    .filter((word) => word !== "");
  return words
    .slice(0, 2)
    .map((word) => graphemes.segment(word)[Symbol.iterator]().next().value?.segment ?? "")
    .join("")
    .toLocaleUpperCase();
}

function Person({ name }: { readonly name: string }) {
  return (
    <span className="flex size-8 shrink-0 items-center justify-center rounded-full border border-rule bg-surface font-medium font-mono text-label text-ink">
      <span aria-hidden="true">{initials(name)}</span>
      <span className="sr-only">{`Signed in as ${name}`}</span>
    </span>
  );
}

const focusRing =
  "outline-none data-focus-visible:outline-2 data-focus-visible:outline-offset-2 data-focus-visible:outline-accent";

function Dot({ online }: { readonly online: boolean }) {
  return (
    <span
      aria-hidden="true"
      className={`size-2 shrink-0 rounded-full ${online ? "bg-success" : "border-2 border-ink-subtle"}`}
    />
  );
}

/** A pill per machine, then how many more; `null` while they load. */
function MachinePills({ machines }: { readonly machines: readonly MachinePill[] | null }) {
  if (machines === null) {
    return null;
  }
  const shown = machines.slice(0, PILLS_SHOWN);
  const more = machines.length - shown.length;
  return (
    <ul aria-label="Machines" className="flex items-center gap-2">
      {shown.map((machine) => (
        <li
          key={machine.id}
          className={`flex h-7 items-center gap-1.5 rounded-control border border-rule px-2.5 text-sm ${machine.online ? "text-ink" : "text-ink-subtle"}`}
        >
          <Dot online={machine.online} />
          {machine.name}
          <span className="sr-only">{machine.online ? ", online" : ", offline"}</span>
        </li>
      ))}
      {more > 0 ? (
        <li className="flex h-7 items-center rounded-control border border-rule px-2.5 text-ink-muted text-sm">
          {`+${more} more`}
        </li>
      ) : null}
    </ul>
  );
}

/** On the phone: how many machines are online, in one pill. */
function MachineCount({ machines }: { readonly machines: readonly MachinePill[] | null }) {
  if (machines === null) {
    return null;
  }
  const online = machines.filter((machine) => machine.online).length;
  return (
    <p className="flex h-7 items-center gap-1.5 rounded-control border border-rule px-2.5 text-ink-muted text-sm">
      <Dot online={online > 0} />
      {machines.length === 0 ? "No machines" : `${online} of ${machines.length} online`}
    </p>
  );
}

export function AppShell({
  workspace,
  person,
  machines,
  navigation,
  children,
}: {
  /** The workspace's name. */
  readonly workspace: string;
  /** The signed-in person's display name. */
  readonly person: string;
  /** The workspace's machines, or `null` while they load. */
  readonly machines: readonly MachinePill[] | null;
  readonly navigation: readonly NavItem[];
  readonly children: ReactNode;
}) {
  return (
    <div className="flex min-h-dvh bg-ground text-ink">
      <nav
        aria-label="Main"
        className="fixed inset-x-0 bottom-0 z-10 border-rule border-t bg-surface pb-[env(safe-area-inset-bottom)] md:sticky md:top-0 md:h-dvh md:w-20 md:shrink-0 md:border-t-0 md:border-r md:pb-0"
      >
        <div className="hidden h-13 items-center justify-center border-rule border-b md:flex">
          <Logo />
        </div>
        <ul className="flex md:flex-col md:items-center md:gap-1 md:pt-3">
          {navigation.map((item) => (
            <li key={item.href} className="flex-1 md:flex-none">
              <Link
                href={item.href}
                {...(item.current ? { "aria-current": "page" } : {})}
                className={`flex min-h-14 flex-col items-center justify-center gap-1.5 font-mono text-label uppercase tracking-label md:w-18 md:rounded-nav ${item.current ? "text-accent md:bg-accent-tint" : "text-ink-muted data-hovered:bg-ground"} ${focusRing}`}
              >
                {item.icon}
                {item.label}
              </Link>
            </li>
          ))}
        </ul>
      </nav>
      <div className="flex min-w-0 flex-1 flex-col">
        <header className="sticky top-0 z-10 flex h-13 shrink-0 items-center gap-3 border-rule border-b bg-surface px-4 md:px-5">
          <p className="min-w-0 flex-1 truncate font-semibold text-md">{workspace}</p>
          <div className="hidden md:block">
            <MachinePills machines={machines} />
          </div>
          <div className="md:hidden">
            <MachineCount machines={machines} />
          </div>
          <Person name={person} />
        </header>
        <main className="flex-1 px-4 pt-5 pb-24 md:px-6 md:pt-6 md:pb-8">{children}</main>
      </div>
    </div>
  );
}
