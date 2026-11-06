// A state as a pill: a dot and a word. The word is always there, so colour is never the only
// signal; the dot is filled for an active state and hollow otherwise.

export type Tone = "success" | "warning" | "danger" | "neutral";

const TONES: Record<Tone, { readonly pill: string; readonly dot: string }> = {
  success: { pill: "bg-success-tint text-success", dot: "bg-success" },
  warning: { pill: "bg-warning-tint text-warning-ink", dot: "bg-warning" },
  danger: { pill: "bg-danger-tint text-danger", dot: "bg-danger" },
  neutral: { pill: "bg-neutral-tint text-ink-muted", dot: "border-2 border-ink-muted" },
};

export function StatusPill({ tone, label }: { readonly tone: Tone; readonly label: string }) {
  const { pill, dot } = TONES[tone];
  return (
    <span
      className={`inline-flex h-6 items-center gap-1.5 whitespace-nowrap rounded-control px-2 font-medium text-sm ${pill}`}
    >
      <span aria-hidden="true" className={`size-2 rounded-full ${dot}`} />
      {label}
    </span>
  );
}
