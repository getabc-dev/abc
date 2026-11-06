// The icons the screens use, drawn as the design canvas draws them: 1.5px strokes in the text's
// colour. Each is decoration beside a word, so it is hidden from assistive technology.

import type { ReactNode } from "react";

function Icon({ size, children }: { readonly size: number; readonly children: ReactNode }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {children}
    </svg>
  );
}

/** The Machines page: a screen and a phone. */
export function MachinesIcon({ size = 16 }: { readonly size?: number }) {
  return (
    <Icon size={size}>
      <rect x="2.5" y="4.5" width="13" height="10" rx="1.5" />
      <path d="M6 18.5h6" />
      <path d="M9 14.5v4" />
      <rect x="17.5" y="8" width="4.5" height="11" rx="1" />
    </Icon>
  );
}

/** A macOS machine: a laptop. */
export function LaptopIcon({ size = 18 }: { readonly size?: number }) {
  return (
    <Icon size={size}>
      <rect x="4.5" y="5" width="15" height="10.5" rx="1.5" />
      <path d="M2.5 19h19" />
    </Icon>
  );
}

/** A Linux machine: a server. */
export function ServerIcon({ size = 18 }: { readonly size?: number }) {
  return (
    <Icon size={size}>
      <rect x="3.5" y="4" width="17" height="7" rx="1.5" />
      <rect x="3.5" y="13" width="17" height="7" rx="1.5" />
      <path d="M7 7.5h.01" />
      <path d="M7 16.5h.01" />
    </Icon>
  );
}
