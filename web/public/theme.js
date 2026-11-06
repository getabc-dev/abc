// @ts-check
// The theme, set before the first paint: index.html loads this file as a classic script in <head>,
// so the page never shows in the wrong theme, and the CSP (`script-src 'self'`) allows it as one
// of the app's own files. It is the one place the theme is decided:
// - the choice, Light, Dark or System, is kept per browser in localStorage (`abc-theme`), System
//   when none is kept;
// - System follows the operating system's `prefers-color-scheme`, live;
// - Dark puts the `dark` class on <html>, whose variables DESIGN.md's dark tokens set;
// - <html data-theme> says the choice, which the app's menu shows (src/theme.ts);
// - the app asks for another choice with an `abc-theme` event whose detail is the choice, and
//   another tab's choice arrives through the `storage` event.
// Its names are in a block, so none of them reaches the page's global scope.

{
  const KEY = "abc-theme";
  const root = document.documentElement;
  const system = window.matchMedia("(prefers-color-scheme: dark)");

  /** @param {unknown} value @returns {"light" | "dark" | "system"} */
  // oxlint-disable-next-line unicorn/consistent-function-scoping -- outside the block it is global
  const parse = (value) => (value === "light" || value === "dark" ? value : "system");

  /** @returns {"light" | "dark" | "system"} */
  const stored = () => {
    try {
      return parse(window.localStorage.getItem(KEY));
    } catch {
      // Storage can be refused (a privacy setting); the system's scheme then decides.
      return "system";
    }
  };

  let choice = stored();
  const render = () => {
    root.setAttribute("data-theme", choice);
    root.classList.toggle("dark", choice === "dark" || (choice === "system" && system.matches));
  };

  render();
  system.addEventListener("change", render);
  window.addEventListener("storage", (event) => {
    if (event.key === KEY) {
      choice = stored();
      render();
    }
  });
  document.addEventListener("abc-theme", (event) => {
    choice = parse(event instanceof CustomEvent ? event.detail : null);
    try {
      if (choice === "system") {
        window.localStorage.removeItem(KEY);
      } else {
        window.localStorage.setItem(KEY, choice);
      }
    } catch {
      // Not kept: the choice holds for this page only.
    }
    render();
  });
}
