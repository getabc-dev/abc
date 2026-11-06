// What every screen must hold however it is shown (DESIGN.md), checked where a screenshot is taken,
// in the light and the dark theme:
// - at phone width, every control is at least 44 by 44 px;
// - no navigation label overflows its entry or the rail;
// - on the desktop, no form field is wider than a reading width;
// - the theme changes colours only: every element sits where it sat in the other theme.

import type { Page } from "@playwright/test";

import { expect, screenshot } from "./abc.ts";

/** A theme, as the operating system says it; the app follows it until a person chooses. */
type Theme = "light" | "dark";

/** DESIGN.md's `--target-touch` and `--breakpoint-md`, and the widest a form field may be. */
const TARGET = 44;
const DESKTOP = 768;
const READING_WIDTH = 600;

/** The controls DESIGN.md's touch-target rule covers. */
const CONTROLS =
  'button, input:not([type="hidden"]), select, textarea, a[href], [role="button"], [role="tab"], [role="menuitem"], [role="menuitemradio"], [role="menuitemcheckbox"]';

/** What on the page breaks DESIGN.md's layout rules, each in words. */
function problems(page: Page): Promise<string[]> {
  return page.evaluate(
    ({ controls, target, desktop, readingWidth }) => {
      const found: string[] = [];
      // oxlint-disable-next-line unicorn/consistent-function-scoping -- page.evaluate sends one function
      const named = (element: Element) =>
        `${element.tagName.toLowerCase()} "${(element.textContent ?? "").trim() || (element.getAttribute("aria-label") ?? "")}"`;
      // oxlint-disable-next-line unicorn/consistent-function-scoping -- page.evaluate sends one function
      const shown = (element: Element) => {
        const box = element.getBoundingClientRect();
        return box.width > 0 && box.height > 0 && getComputedStyle(element).visibility !== "hidden";
      };
      const nav = document.querySelector('nav[aria-label="Main"]');
      if (nav !== null) {
        const rail = nav.getBoundingClientRect();
        for (const link of nav.querySelectorAll("a")) {
          const label = link.querySelector("span:last-child");
          if (label === null) {
            continue;
          }
          const box = label.getBoundingClientRect();
          const entry = link.getBoundingClientRect();
          if (
            label.scrollWidth > label.clientWidth ||
            box.left < entry.left ||
            box.right > entry.right ||
            entry.left < rail.left ||
            entry.right > rail.right
          ) {
            found.push(`the label ${named(label)} overflows its entry or the rail`);
          }
        }
      }
      for (const control of document.querySelectorAll(controls)) {
        if (!shown(control)) {
          continue;
        }
        const box = control.getBoundingClientRect();
        // Links inside running text are inline, and WCAG 2.5.8 exempts them.
        const inline = getComputedStyle(control).display === "inline";
        if (window.innerWidth < desktop && !inline && (box.width < target || box.height < target)) {
          found.push(`${named(control)} is ${Math.round(box.width)}×${Math.round(box.height)} px`);
        }
        if (
          window.innerWidth >= desktop &&
          control.tagName === "INPUT" &&
          box.width > readingWidth
        ) {
          found.push(`${named(control)} is ${Math.round(box.width)} px wide`);
        }
      }
      return found;
    },
    { controls: CONTROLS, target: TARGET, desktop: DESKTOP, readingWidth: READING_WIDTH },
  );
}

/** Where every element of the page is, by its place in the document. */
function layout(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    [...document.body.querySelectorAll("*")].map((element) => {
      const { x, y, width, height } = element.getBoundingClientRect();
      return `${element.tagName} ${x.toFixed(1)} ${y.toFixed(1)} ${width.toFixed(1)} ${height.toFixed(1)}`;
    }),
  );
}

/** Shows the page in `theme` and waits until it is applied. */
async function showIn(page: Page, theme: Theme): Promise<void> {
  await page.emulateMedia({ colorScheme: theme });
  await expect
    .poll(() => page.evaluate(() => document.documentElement.classList.contains("dark")))
    .toBe(theme === "dark");
}

/** Checks and saves the page in `theme`, and says where everything sits. */
async function look(page: Page, name: string, theme: Theme): Promise<string[]> {
  await showIn(page, theme);
  // Measured at rest: a menu opens with a zoom, which shrinks its items until it ends. An animation
  // cancelled meanwhile, such as a colour transition the theme change replaced, is at rest too.
  await page.evaluate(() =>
    Promise.all(
      document.getAnimations().map((animation) => animation.finished.catch(() => undefined)),
    ),
  );
  expect(await problems(page), `${name} in the ${theme} theme`).toEqual([]);
  const places = await layout(page);
  // Transitions end at once, so a colour is never caught halfway.
  await page.screenshot({
    path: screenshot(`${name}-${theme}`),
    fullPage: true,
    animations: "disabled",
  });
  return places;
}

/**
 * Checks the page as it is shown now against DESIGN.md's rules, in both themes, and saves it as
 * `target/e2e-screens/<name>-light.png` and `<name>-dark.png`.
 */
export async function capture(page: Page, name: string): Promise<void> {
  // One theme after the other: the page shows one at a time.
  const light = await look(page, name, "light");
  const dark = await look(page, name, "dark");
  expect(dark, `${name}: the dark theme moved something`).toEqual(light);
  await showIn(page, "light");
}
