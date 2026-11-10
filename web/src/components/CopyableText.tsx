// A command or secret a person pastes elsewhere, in a mono well with a Copy button, built from
// shadcn's Button: shadcn's registry has no copy control but `code-block`, which brings the shiki
// highlighter for what is plain text (decision 46). The value shows on one line, as it is pasted:
// a URL or token never breaks mid-word, and one wider than its box scrolls inside it, never
// clipped, while the Copy button stays in view beside it. Copy puts exactly the value on the
// clipboard, with no prompt or newline, then says "Copied" for a moment, which screen readers hear.
// Where the clipboard is refused, the value is selected instead, for the person to copy with their
// keyboard.

import { CheckIcon, CopyIcon } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { Button } from "@/components/ui/button";

/** How long "Copied" shows. */
const SHOWN_MS = 2000;

type Outcome = "idle" | "copied" | "selected";

export function CopyableText({
  value,
  label,
}: {
  /** The exact text to show and copy. */
  readonly value: string;
  /** What the value is, for the button's accessible name: "the command" becomes "Copy the command". */
  readonly label: string;
}) {
  const text = useRef<HTMLElement>(null);
  const [outcome, setOutcome] = useState<Outcome>("idle");

  useEffect(() => {
    if (outcome !== "copied") {
      return;
    }
    const timer = setTimeout(() => {
      setOutcome("idle");
    }, SHOWN_MS);
    return () => {
      clearTimeout(timer);
    };
  }, [outcome]);

  const select = () => {
    const code = text.current;
    if (code !== null) {
      window.getSelection()?.selectAllChildren(code);
    }
    setOutcome("selected");
  };

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(value);
      setOutcome("copied");
    } catch {
      select();
    }
  };

  return (
    <div className="mt-1.5 flex items-center gap-1 rounded-lg border bg-sunken py-1 pr-1 pl-3">
      <pre className="min-w-0 flex-1 overflow-x-auto whitespace-pre py-1 font-mono text-foreground text-xs">
        <code ref={text}>{value}</code>
      </pre>
      <Button
        variant="ghost"
        size="sm"
        className="shrink-0"
        aria-label={`Copy ${label}`}
        onClick={() => {
          void copy();
        }}
      >
        {outcome === "copied" ? <CheckIcon aria-hidden="true" /> : <CopyIcon aria-hidden="true" />}
        <span aria-hidden="true">{outcome === "copied" ? "Copied" : "Copy"}</span>
      </Button>
      <output className="sr-only">
        {outcome === "copied"
          ? `Copied ${label}`
          : outcome === "selected"
            ? `Selected ${label}; copy it with your keyboard`
            : ""}
      </output>
    </div>
  );
}
