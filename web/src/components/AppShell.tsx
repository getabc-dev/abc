// The frame of every signed-in page, after the canvas's Home A, from shadcn's Sidebar, Badge,
// Avatar and DropdownMenu. On the desktop: the rail (the logo and the navigation) and the header
// (the workspace, a pill per machine, the theme and the person). On the phone, after the canvas's
// phone Home: a top bar (the workspace, the machines online, the theme and the person), the page,
// and the same navigation as a tab bar at the bottom. One Sidebar is both the rail and the tab bar,
// so there is one landmark and one set of links whatever the width.
//
// The rail is as wide as its widest entry and never narrower than the canvas's, so a label never
// overflows it; the end-to-end layout check holds that.

import { Link } from "@tanstack/react-router";
import type { ReactNode } from "react";

import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import {
  Sidebar,
  SidebarContent,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
} from "@/components/ui/sidebar";

import { Logo } from "./Logo";
import { ThemeMenu } from "./ThemeMenu";

export type NavItem = {
  readonly to: "/machines";
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
    <Avatar>
      <AvatarFallback className="font-medium font-mono text-2xs">
        <span aria-hidden="true">{initials(name)}</span>
        <span className="sr-only">{`Signed in as ${name}`}</span>
      </AvatarFallback>
    </Avatar>
  );
}

function Dot({ online }: { readonly online: boolean }) {
  return (
    <span
      aria-hidden="true"
      className={`size-2 shrink-0 rounded-full ${online ? "bg-success" : "border-2 border-subtle-foreground"}`}
    />
  );
}

const pill = "h-7 gap-1.5 rounded-lg px-2.5 text-xs";

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
        <li key={machine.id}>
          <Badge
            variant="outline"
            className={`${pill} ${machine.online ? "text-foreground" : "text-subtle-foreground"}`}
          >
            <Dot online={machine.online} />
            {machine.name}
            <span className="sr-only">{machine.online ? ", online" : ", offline"}</span>
          </Badge>
        </li>
      ))}
      {more > 0 ? (
        <li>
          <Badge variant="outline" className={`${pill} text-muted-foreground`}>
            {`+${more} more`}
          </Badge>
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
    <Badge variant="outline" className={`${pill} text-muted-foreground`}>
      <Dot online={online > 0} />
      {machines.length === 0 ? "No machines" : `${online} of ${machines.length} online`}
    </Badge>
  );
}

export function AppShell({
  workspace,
  person,
  machines,
  navigation,
  notice,
  children,
}: {
  /** The workspace's name. */
  readonly workspace: string;
  /** The signed-in person's display name. */
  readonly person: string;
  /** The workspace's machines, or `null` while they load. */
  readonly machines: readonly MachinePill[] | null;
  readonly navigation: readonly NavItem[];
  /** A line under the header about the whole app, or `null`. */
  readonly notice: ReactNode;
  readonly children: ReactNode;
}) {
  return (
    <SidebarProvider className="bg-background text-foreground">
      <nav
        aria-label="Main"
        className="fixed inset-x-0 bottom-0 z-10 border-t pb-[env(safe-area-inset-bottom)] md:sticky md:top-0 md:h-svh md:shrink-0 md:border-t-0 md:border-r md:pb-0"
      >
        <Sidebar collapsible="none" className="h-auto w-full md:h-full md:w-max md:min-w-20">
          <SidebarHeader className="hidden h-13 items-center justify-center border-b p-0 md:flex">
            <Logo />
          </SidebarHeader>
          <SidebarContent className="md:px-1 md:pt-3">
            <SidebarMenu className="flex-row md:flex-col md:gap-1">
              {navigation.map((item) => (
                <SidebarMenuItem key={item.to} className="flex-1 md:flex-none">
                  <SidebarMenuButton
                    isActive={item.current}
                    render={
                      <Link to={item.to} {...(item.current ? { "aria-current": "page" } : {})} />
                    }
                    className="h-auto min-h-14 flex-col justify-center gap-1.5 overflow-visible rounded-none px-2 text-center font-mono text-2xs uppercase tracking-label md:rounded-lg"
                  >
                    {item.icon}
                    <span>{item.label}</span>
                  </SidebarMenuButton>
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarContent>
        </Sidebar>
      </nav>
      <div className="flex min-w-0 flex-1 flex-col">
        <header className="sticky top-0 z-10 flex h-13 shrink-0 items-center gap-3 border-b bg-card px-4 md:px-5">
          <p className="min-w-0 flex-1 truncate font-semibold text-base">{workspace}</p>
          <div className="hidden md:block">
            <MachinePills machines={machines} />
          </div>
          <div className="md:hidden">
            <MachineCount machines={machines} />
          </div>
          <ThemeMenu />
          <Person name={person} />
        </header>
        {notice}
        <main className="flex-1 px-4 pt-5 pb-24 md:px-6 md:pt-6 md:pb-8">{children}</main>
      </div>
    </SidebarProvider>
  );
}
