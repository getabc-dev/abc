// abc's text field: a React Aria TextField with its label, description and error, which React
// Aria ties to the input so a screen reader reads them with it.

import {
  TextField as AriaTextField,
  FieldError,
  Input,
  Label,
  Text,
  type TextFieldProps,
} from "react-aria-components";

export function TextField({
  label,
  description,
  error,
  mono = false,
  ...props
}: Omit<TextFieldProps, "className" | "isInvalid" | "validationBehavior" | "children"> & {
  readonly label: string;
  readonly description?: string;
  /** What is wrong with the value, shown under the field; none when it is fine. */
  readonly error?: string | undefined;
  /** For codes and identifiers, set in the mono font. */
  readonly mono?: boolean;
}) {
  return (
    <AriaTextField
      {...props}
      isInvalid={error !== undefined}
      validationBehavior="aria"
      className="flex flex-col gap-1.5"
    >
      <Label className="font-medium text-base text-ink">{label}</Label>
      <Input
        className={[
          "min-h-9 rounded-control border border-control bg-sunken px-3 text-base text-ink outline-none pointer-coarse:min-h-11",
          "data-focused:border-accent data-focused:shadow-halo data-invalid:border-danger",
          mono ? "font-mono" : "",
        ].join(" ")}
      />
      {description === undefined ? null : (
        <Text slot="description" className="text-ink-muted text-sm">
          {description}
        </Text>
      )}
      <FieldError className="text-danger text-sm">{error}</FieldError>
    </AriaTextField>
  );
}
