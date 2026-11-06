// shadcn's Card in a region named by its heading, at the level the page needs, with its state, if
// any, as a Status beside the title.

import { type ReactNode, useId } from "react";

import { Card, CardAction, CardContent, CardHeader, CardTitle } from "@/components/ui/card";

import { Status, type Tone } from "./Status";

export function TitledCard({
  title,
  level = 2,
  status,
  children,
  className,
}: {
  readonly title: ReactNode;
  readonly level?: 1 | 2 | 3;
  /** A state, as a Status beside the title. */
  readonly status?: { readonly tone: Tone; readonly label: string };
  readonly children?: ReactNode;
  readonly className?: string | undefined;
}) {
  const heading = useId();
  const Heading = `h${level}` as const;
  return (
    <section aria-labelledby={heading} className={className}>
      <Card>
        <CardHeader>
          <CardTitle>
            <Heading id={heading}>{title}</Heading>
          </CardTitle>
          {status === undefined ? null : (
            <CardAction>
              <Status tone={status.tone} label={status.label} />
            </CardAction>
          )}
        </CardHeader>
        <CardContent>{children}</CardContent>
      </Card>
    </section>
  );
}
