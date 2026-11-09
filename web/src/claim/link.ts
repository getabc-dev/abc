// The claim link `abc server setup` opens: `https://<server>/#code=<setup code>`. The code rides in
// the fragment, which the browser never sends, so no server or proxy log can hold it. The app reads
// it once, as it starts, and takes it out of the address bar and the history entry.

/** The setup code in a location's fragment (`#code=…`), or "" when it holds none. */
export function setupCodeFrom(hash: string): string {
  const code = new URLSearchParams(hash.replace(/^#/, "")).get("code");
  return code?.trim() ?? "";
}

type Location = { readonly hash: string; readonly pathname: string; readonly search: string };
type History = { readonly replaceState: (data: unknown, unused: string, url: string) => void };

/** Reads the setup code from `location`'s fragment, and replaces the entry with one without it. */
export function takeSetupCode(location: Location, history: History): string {
  const code = setupCodeFrom(location.hash);
  if (code !== "") {
    history.replaceState(null, "", `${location.pathname}${location.search}`);
  }
  return code;
}
