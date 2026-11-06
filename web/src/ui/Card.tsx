// A card: a surface with a rule border, as a region named by its heading, with its state as a pill.

import { type ReactNode, useId } from "react";
import { Heading } from "react-aria-components";

import { StatusPill, type Tone } from "./StatusPill";

export function Card({
  title,
  level = 2,
  status,
  children,
  className,
}: {
  readonly title: ReactNode;
  readonly level?: 1 | 2 | 3;
  /** A state, as a pill beside the title. */
  readonly status?: { readonly tone: Tone; readonly label: string };
  readonly children?: ReactNode;
  readonly className?: string;
}) {
  const heading = useId();
  return (
    <section
      aria-labelledby={heading}
      className={`rounded-card border border-rule bg-surface ${className ?? ""}`}
    >
      <div className="flex items-start justify-between gap-3 px-5 pt-4">
        <Heading id={heading} level={level} className="font-semibold text-ink text-md">
          {title}
        </Heading>
        {status === undefined ? null : <StatusPill tone={status.tone} label={status.label} />}
      </div>
      <div className="px-5 pt-3 pb-5">{children}</div>
    </section>
  );
}
