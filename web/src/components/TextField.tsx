// A text field from shadcn's Field pieces and Input: its label, description and error, tied to the
// input by id so a screen reader reads them with it. shadcn's Field leaves that wiring to its user;
// this is the one place abc does it.

import { type ComponentProps, useId } from "react";

import { Field, FieldDescription, FieldError, FieldLabel } from "@/components/ui/field";
import { Input } from "@/components/ui/input";

export function TextField({
  label,
  description,
  error,
  mono = false,
  onChange,
  ...props
}: Omit<ComponentProps<typeof Input>, "id" | "onChange" | "aria-invalid" | "aria-describedby"> & {
  readonly label: string;
  readonly description?: string;
  /** What is wrong with the value, shown under the field; none when it is fine. */
  readonly error?: string | undefined;
  /** For codes and identifiers, set in the mono font. */
  readonly mono?: boolean;
  readonly onChange: (value: string) => void;
}) {
  const id = useId();
  const describedBy = [
    description === undefined ? null : `${id}-description`,
    error === undefined ? null : `${id}-error`,
  ].filter((part) => part !== null);
  return (
    <Field data-invalid={error !== undefined}>
      <FieldLabel htmlFor={id}>{label}</FieldLabel>
      <Input
        {...props}
        id={id}
        className={mono ? "font-mono" : undefined}
        aria-invalid={error !== undefined}
        aria-describedby={describedBy.length === 0 ? undefined : describedBy.join(" ")}
        onChange={(event) => {
          onChange(event.currentTarget.value);
        }}
      />
      {description === undefined ? null : (
        <FieldDescription id={`${id}-description`}>{description}</FieldDescription>
      )}
      {error === undefined ? null : <FieldError id={`${id}-error`}>{error}</FieldError>}
    </Field>
  );
}
