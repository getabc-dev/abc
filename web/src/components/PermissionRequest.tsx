// A permission request: someone or something asks for access, and the owner answers. The concept
// is AffectUI's "permission request" (https://www.affectui.com/components; ideas only, no code of
// theirs): say who asks and for what, show the details that tell a real request from a fake, and
// put the two answers, Approve and Deny, side by side with nothing between the person and them.
// Built from shadcn's Card, Badge, Alert and Button, so it takes both themes from the tokens.
//
// abc's request is a machine asking to become a worker. The code is the one thing the owner
// compares with the machine's own terminal: it proves this page is that terminal's request.

import { LaptopIcon, ServerIcon, ShieldCheckIcon } from "lucide-react";
import type { ReactNode } from "react";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardFooter, CardHeader, CardTitle } from "@/components/ui/card";

import type { Os } from "../generated/Os";

export type Detail = { readonly label: string; readonly value: ReactNode };

export function PermissionRequest({
  asker,
  os,
  asks,
  details,
  code,
  codeHelp,
  expires,
  error,
  deciding,
  onApprove,
  onDeny,
}: {
  /** Who asks: the machine's name. */
  readonly asker: string;
  readonly os: Os;
  /** What it asks for, in a sentence. */
  readonly asks: string;
  /** What tells this request from another: a list of labelled facts. */
  readonly details: readonly Detail[];
  /** The code the asker shows on its own screen. */
  readonly code: string;
  /** Why the owner compares it. */
  readonly codeHelp: string;
  /** When the request lapses, in words. */
  readonly expires: string;
  /** Why the last answer failed, if it did. */
  readonly error: string | null;
  /** The answer being sent; both buttons wait while one is. */
  readonly deciding: "approve" | "deny" | null;
  readonly onApprove: () => void;
  readonly onDeny: () => void;
}) {
  const busy = deciding !== null;
  return (
    <Card className="w-full max-w-xl">
      <CardHeader>
        <div className="flex items-center gap-3">
          <span className="flex size-10 shrink-0 items-center justify-center rounded-lg border bg-sunken text-muted-foreground">
            {os === "macos" ? (
              <LaptopIcon aria-hidden="true" className="size-5" />
            ) : (
              <ServerIcon aria-hidden="true" className="size-5" />
            )}
          </span>
          <div className="min-w-0">
            <CardTitle>
              <h1 className="truncate">{asker} wants to join</h1>
            </CardTitle>
            <p className="text-muted-foreground text-xs">{asks}</p>
          </div>
        </div>
      </CardHeader>
      <CardContent className="flex flex-col gap-5">
        <div className="rounded-lg border bg-sunken px-4 py-3 text-center">
          <p className="font-mono text-2xs text-subtle-foreground uppercase tracking-label">
            Code on the machine
          </p>
          <p className="mt-1 font-mono font-semibold text-2xl tracking-widest">{code}</p>
          <p className="mt-1 text-muted-foreground text-xs">{codeHelp}</p>
        </div>
        <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1.5 text-xs">
          {details.map((detail) => (
            <div key={detail.label} className="contents">
              <dt className="font-mono text-2xs text-subtle-foreground uppercase tracking-label">
                {detail.label}
              </dt>
              <dd className="min-w-0 break-words text-foreground">{detail.value}</dd>
            </div>
          ))}
        </dl>
        {error === null ? null : (
          <Alert variant="destructive" className="bg-destructive-tint">
            <AlertDescription>{error}</AlertDescription>
          </Alert>
        )}
      </CardContent>
      <CardFooter className="flex flex-wrap items-center gap-2">
        <Button disabled={busy} focusableWhenDisabled onClick={onApprove}>
          <ShieldCheckIcon aria-hidden="true" data-icon="inline-start" />
          {deciding === "approve" ? "Approving…" : "Approve"}
        </Button>
        <Button variant="outline" disabled={busy} focusableWhenDisabled onClick={onDeny}>
          {deciding === "deny" ? "Denying…" : "Deny"}
        </Button>
        <p className="ml-auto text-muted-foreground text-xs">{expires}</p>
      </CardFooter>
    </Card>
  );
}
