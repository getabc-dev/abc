// The frame of the pages before the shell: the claim and the visitor's standing. One calm column
// centred on the ground, under the logo.

import type { ReactNode } from "react";

import { Logo } from "../ui/Logo";

export function Centered({ children }: { readonly children: ReactNode }) {
  return (
    <main className="flex min-h-dvh flex-col items-center bg-ground px-4 py-10 text-ink md:justify-center">
      <div className="flex w-full max-w-md flex-col gap-6">
        <div className="self-center">
          <Logo />
        </div>
        {children}
      </div>
    </main>
  );
}
