// abc's button: a React Aria Button with the look of DESIGN.md.

import { Button as AriaButton, type ButtonProps } from "react-aria-components";

const VARIANTS = {
  /** The screen's main action: accent fill. */
  primary:
    "bg-accent text-surface data-hovered:bg-accent-strong data-pressed:bg-accent-strong border border-transparent",
  /** Any other action: a surface with a rule border. */
  secondary:
    "bg-surface text-ink border border-rule-strong data-hovered:bg-sunken data-pressed:bg-ground",
} as const;

export type ButtonVariant = keyof typeof VARIANTS;

export function Button({
  variant = "secondary",
  className,
  ...props
}: Omit<ButtonProps, "className"> & {
  readonly variant?: ButtonVariant;
  readonly className?: string;
}) {
  return (
    <AriaButton
      {...props}
      className={[
        "inline-flex min-h-8 cursor-default items-center justify-center gap-2 rounded-control px-4 font-medium text-base",
        "outline-none data-focus-visible:outline-2 data-focus-visible:outline-offset-2 data-focus-visible:outline-accent",
        "data-disabled:opacity-50 data-pending:opacity-70 pointer-coarse:min-h-11",
        VARIANTS[variant],
        className ?? "",
      ].join(" ")}
    />
  );
}
