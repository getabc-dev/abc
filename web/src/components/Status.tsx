// A state as shadcn's Badge: a dot and a word. The word is always there, so colour is never the
// only signal; the dot is filled for an active state and hollow otherwise.

import { Badge } from "@/components/ui/badge";

export type Tone = "success" | "warning" | "danger" | "neutral";

const TONES: Record<Tone, { readonly badge: string; readonly dot: string }> = {
  success: { badge: "bg-success-tint text-success", dot: "bg-success" },
  warning: { badge: "bg-warning-tint text-warning-foreground", dot: "bg-warning" },
  danger: { badge: "bg-destructive-tint text-destructive", dot: "bg-destructive" },
  neutral: { badge: "bg-muted text-muted-foreground", dot: "border-2 border-muted-foreground" },
};

export function Status({ tone, label }: { readonly tone: Tone; readonly label: string }) {
  const { badge, dot } = TONES[tone];
  return (
    <Badge className={`h-6 gap-1.5 rounded-lg px-2 ${badge}`}>
      <span aria-hidden="true" className={`size-2 rounded-full ${dot}`} />
      {label}
    </Badge>
  );
}
