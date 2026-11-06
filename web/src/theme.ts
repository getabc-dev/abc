// The theme as the app sees it. public/theme.js decides and applies it, before the first paint and
// on every change: it says the choice on <html data-theme>, and takes a new one from an
// `abc-theme` event. This module reads the one and sends the other, so the rule lives in one place.

import { useSyncExternalStore } from "react";

/** The choices, in the menu's order. public/theme.js cannot import them; a test keeps it in step. */
export const THEME_CHOICES = ["light", "dark", "system"] as const;

export type ThemeChoice = (typeof THEME_CHOICES)[number];

export function isThemeChoice(value: unknown): value is ThemeChoice {
  return THEME_CHOICES.some((choice) => choice === value);
}

/** The theme chosen in this browser, as public/theme.js says it on <html>. */
export function themeChoice(): ThemeChoice {
  const said = document.documentElement.getAttribute("data-theme");
  return isThemeChoice(said) ? said : "system";
}

/** Asks public/theme.js to apply and keep `choice`. */
export function chooseTheme(choice: ThemeChoice): void {
  document.dispatchEvent(new CustomEvent("abc-theme", { detail: choice }));
}

function subscribe(changed: () => void): () => void {
  const observer = new MutationObserver(changed);
  observer.observe(document.documentElement, { attributeFilter: ["data-theme"] });
  return () => {
    observer.disconnect();
  };
}

/** The theme chosen, kept current when it changes here or in another tab. */
export function useThemeChoice(): ThemeChoice {
  return useSyncExternalStore(subscribe, themeChoice);
}
