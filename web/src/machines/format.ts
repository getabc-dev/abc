// How the Machines screen words a machine's facts.

import type { Arch } from "../generated/Arch";
import type { MachineCard } from "../generated/MachineCard";
import type { Os } from "../generated/Os";
import type { Timestamp } from "../generated/Timestamp";

const OS_NAMES: Record<Os, string> = { macos: "macOS", linux: "Linux" };

const ARCH_NAMES: Record<Arch, string> = { aarch64: "ARM64", x86_64: "x86-64" };

/** The machine's OS and architecture, such as "macOS on ARM64". */
export function describeMachine(machine: Pick<MachineCard, "os" | "arch">): string {
  return `${OS_NAMES[machine.os]} on ${ARCH_NAMES[machine.arch]}`;
}

/**
 * An instant as a date and time, in the person's locale and time zone unless `locale` and
 * `timeZone` say otherwise (tests do, so they do not depend on the machine's).
 */
export function formatInstant(
  instant: Timestamp,
  { locale, timeZone }: { readonly locale?: string; readonly timeZone?: string } = {},
): string {
  return new Intl.DateTimeFormat(locale, {
    dateStyle: "medium",
    timeStyle: "short",
    ...(timeZone === undefined ? {} : { timeZone }),
  }).format(new Date(instant));
}
