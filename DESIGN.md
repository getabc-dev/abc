# abc's design

How abc looks, in one place. The look comes from the design canvas the maintainer approved
(S0, November 2026): a calm, flat interface; Inter for what people read and JetBrains Mono for
labels and identifiers, with small uppercase mono labels as part of the look. The canvas is light;
the dark theme is designed here with the same care, not inverted.

The tokens below are the only source of the look. `web/src/styles.css` declares them: the colours
as variables, light on `:root` and dark on `.dark`, which `@theme inline` turns into Tailwind's
utilities (`bg-card`, `text-muted-foreground`), and the rest in `@theme`.
`web/src/design.test.ts` reads this file and the stylesheet and fails when the two differ, when a
pair listed under [Contrast](#contrast) falls short in either theme, or when a component writes a
colour of its own. Change a token here and in the stylesheet together.

The colour names are shadcn/ui's, so its components (below) take abc's look with no edits; abc adds
`sunken`, `subtle-foreground`, `overlay` and the `success`, `warning` and `destructive-tint`
tokens. `@theme` resets Tailwind's own colours, fonts, text sizes, radii and shadows
(`--color-*: initial` and so on), so a utility outside this list does not exist.

## Colour

| Token | Light | Dark | Purpose |
| --- | --- | --- | --- |
| `--background` | `#F7F7F5` | `#161615` | The page behind everything (the canvas's ground). |
| `--foreground` | `#1C1C1A` | `#EDECE8` | Primary text, and the logo tile. |
| `--card` | `#FFFFFF` | `#1F1F1D` | Cards and the header. |
| `--card-foreground` | `#1C1C1A` | `#EDECE8` | Text on a card. |
| `--popover` | `#FFFFFF` | `#262624` | Menus and other surfaces that float over the page. |
| `--popover-foreground` | `#1C1C1A` | `#EDECE8` | Text in a menu. |
| `--sunken` | `#FBFBFA` | `#1A1A19` | Wells inside a surface: commands and the machine icon. |
| `--primary` | `#3451D1` | `#8FA1F7` | Primary actions and links. |
| `--primary-foreground` | `#FFFFFF` | `#111733` | Text on a primary action. |
| `--secondary` | `#EEEDE9` | `#2B2B28` | Secondary actions' fill. |
| `--secondary-foreground` | `#1C1C1A` | `#EDECE8` | Text on a secondary action. |
| `--muted` | `#EEEDE9` | `#2B2B28` | Hovered rows and the neutral pill. |
| `--muted-foreground` | `#5F5E59` | `#ABA9A2` | Secondary text, and text on the neutral pill. |
| `--subtle-foreground` | `#6E6D68` | `#9C9A93` | Mono labels and metadata, on the background, a card or a well only. |
| `--accent` | `#EBEEFA` | `#262D4D` | A highlighted menu item. |
| `--accent-foreground` | `#2639A3` | `#C9D1FB` | Text of a highlighted menu item. |
| `--destructive` | `#B3261E` | `#F28B82` | Errors, refusals and revoked machines: text, dots and edges. |
| `--destructive-tint` | `#F7E9E8` | `#3B201D` | The background of a danger pill or banner. |
| `--success` | `#1F7A3A` | `#6DCB8C` | Online and healthy: text and dots. |
| `--success-tint` | `#E9F2EB` | `#17301F` | The background of a success pill. |
| `--warning` | `#B35400` | `#E8994A` | Reconnecting and stale: dots and edges. |
| `--warning-foreground` | `#934500` | `#F2B171` | Warning text on the warning tint. |
| `--warning-tint` | `#F7EEE6` | `#382818` | The background of a warning pill or banner. |
| `--border` | `#E6E5E1` | `#33332F` | Dividers and card edges: decoration, never a control's only edge. |
| `--input` | `#878680` | `#7C7A74` | A text field's edge: 3:1 against every surface, as controls need. |
| `--ring` | `#3451D1` | `#8FA1F7` | The focus ring. |
| `--overlay` | `#1C1C1A` | `#000000` | The scrim behind a sheet, at 10%. |
| `--sidebar` | `#FFFFFF` | `#1F1F1D` | The rail and the tab bar. |
| `--sidebar-foreground` | `#5F5E59` | `#ABA9A2` | Navigation labels and icons. |
| `--sidebar-accent` | `#EBEEFA` | `#262D4D` | The current page's background in the navigation. |
| `--sidebar-accent-foreground` | `#3451D1` | `#AEBBFA` | The current page's label in the navigation. |
| `--sidebar-border` | `#E6E5E1` | `#33332F` | The rail's edge. |
| `--sidebar-ring` | `#3451D1` | `#8FA1F7` | Focus in the navigation. |

The canvas draws text fields with the rule colour, a 1.2:1 edge, so fields use `--input`. It sets
warning labels in the warning colour on its tint (4.4:1); they use `--warning-foreground`.

## Themes

The app works in light and dark. It follows the operating system's `prefers-color-scheme` until the
person chooses Light, Dark or System in the shell's theme menu; the choice is kept per browser
(`localStorage`). `web/public/theme.js`, a classic script in `index.html`'s head, applies it before
the first paint, so no page shows in the wrong theme; it is one of the app's own files, as the CSP
(`script-src 'self'`) requires. Dark is the `dark` class on `<html>`, which sets the dark values
above. A theme changes colours only, never layout; the end-to-end tests check both.

## Type

| Token | Value | Purpose |
| --- | --- | --- |
| `--font-sans` | `"Inter Variable", ui-sans-serif, system-ui, sans-serif` | Everything people read. |
| `--font-mono` | `"JetBrains Mono Variable", ui-monospace, monospace` | Labels, identifiers, commands and codes. |
| `--text-2xs` | `11px` | Small uppercase mono labels (`font-mono uppercase tracking-label`). |
| `--text-2xs--line-height` | `16px` | Their line height. |
| `--text-xs` | `12px` | Metadata, pills, commands and secondary lines. |
| `--text-xs--line-height` | `16px` | Its line height. |
| `--text-sm` | `13px` | Body text, buttons and fields: shadcn's components' size. |
| `--text-sm--line-height` | `20px` | Its line height. |
| `--text-base` | `15px` | Card titles, the workspace name, and fields on the phone. |
| `--text-base--line-height` | `22px` | Its line height. |
| `--text-xl` | `22px` | Page titles. |
| `--text-xl--line-height` | `28px` | Its line height. |
| `--tracking-label` | `0.06em` | The letter spacing of uppercase labels. |

The sizes keep Tailwind's names (`2xs` to `xl`): `cn`, which merges class names in shadcn's
components, tells a size from a colour by those names, so a custom size name would be dropped
beside a colour. The fonts are self-hosted from the `@fontsource-variable/inter` and
`@fontsource-variable/jetbrains-mono` packages, since the app's CSP is `default-src 'self'`.
Weights are 400 for text, 500 for labels and buttons, and 600 for titles. The root font size stays
the browser's 16px, so a rem is 16px and every spacing and size here means what it says.

## Space, radius and elevation

| Token | Value | Purpose |
| --- | --- | --- |
| `--spacing` | `0.25rem` | The spacing step: `p-4` is 16px. Layouts use steps of 1 to 6. |
| `--radius-sm` | `3px` | Chips and inline codes. |
| `--radius-md` | `4px` | Menu items and tooltips. |
| `--radius-lg` | `6px` | Buttons, fields, pills, banners and navigation entries. |
| `--radius-xl` | `12px` | Cards. |
| `--radius-4xl` | `9999px` | shadcn's round badges; abc's pills use `--radius-lg`. |
| `--shadow-sm` | `0 1px 2px rgb(0 0 0 / 0.06)` | shadcn's sidebar inset, unused by abc's layout. |
| `--shadow-md` | `0 4px 12px rgb(0 0 0 / 0.1)` | Menus, which float over the page. |
| `--shadow-lg` | `0 12px 32px rgb(0 0 0 / 0.16)` | Sheets and submenus. |
| `--breakpoint-md` | `48rem` | Below it the phone layout (top bar and tab bar), from it the desktop one (rail and header). |
| `--target-touch` | `2.75rem` | The least height and width of a control below `md`: 44px. |

abc is flat: surfaces are set apart by edges, never shadows. Only what floats over the page, a menu
or a sheet, casts one.

## Contrast

Every pair of text and background the components use, at least 4.5:1 in both themes (WCAG 2.2 AA
for normal text). The test computes each ratio from the tokens above.

| Text | Background |
| --- | --- |
| `--foreground` | `--background` |
| `--foreground` | `--card` |
| `--foreground` | `--popover` |
| `--foreground` | `--sunken` |
| `--foreground` | `--muted` |
| `--foreground` | `--secondary` |
| `--foreground` | `--accent` |
| `--muted-foreground` | `--background` |
| `--muted-foreground` | `--card` |
| `--muted-foreground` | `--popover` |
| `--muted-foreground` | `--sunken` |
| `--muted-foreground` | `--muted` |
| `--subtle-foreground` | `--background` |
| `--subtle-foreground` | `--card` |
| `--subtle-foreground` | `--sunken` |
| `--card-foreground` | `--card` |
| `--popover-foreground` | `--popover` |
| `--secondary-foreground` | `--secondary` |
| `--primary` | `--card` |
| `--primary` | `--background` |
| `--primary-foreground` | `--primary` |
| `--background` | `--foreground` |
| `--success` | `--success-tint` |
| `--success` | `--card` |
| `--warning-foreground` | `--warning-tint` |
| `--destructive` | `--card` |
| `--destructive` | `--destructive-tint` |
| `--destructive` | `--background` |
| `--accent-foreground` | `--accent` |
| `--sidebar-foreground` | `--sidebar` |
| `--sidebar-accent-foreground` | `--sidebar-accent` |

Every edge a person must see to use a control, and the focus ring, at least 3:1 against what it
sits on in both themes (WCAG 2.2 non-text contrast).

| Edge | Background |
| --- | --- |
| `--input` | `--background` |
| `--input` | `--card` |
| `--input` | `--popover` |
| `--input` | `--sunken` |
| `--ring` | `--background` |
| `--ring` | `--card` |
| `--ring` | `--popover` |
| `--ring` | `--sidebar` |
| `--sidebar-ring` | `--sidebar` |
| `--sidebar-ring` | `--sidebar-accent` |

## Components

abc's web UI is **shadcn/ui on Base UI primitives** (decision 46). Base UI brings the behaviour,
keyboard handling and ARIA; shadcn's registry brings the components; DESIGN.md's tokens bring the
look.

- **A component shadcn offers is taken from its registry, never hand-built.** Add it with the
  pinned CLI from the repository's root: `cargo xtask pnpm exec shadcn add <name>`.
  `web/components.json` sets the `base-nova` style (Base UI, compact), Tailwind v4 with
  `src/styles.css`, lucide icons, and `src/components/ui/` for registry files.
- **The registry files are ours, kept as the CLI writes them.** Biome and Oxlint skip
  `src/components/ui/` and `src/hooks/use-mobile.ts`, so `shadcn add --overwrite` shows only what
  changed upstream; `tsc`, knip and the tests cover them. They use tokens only. abc's edits, which
  a refresh must keep:
  - focus rings are `ring-ring`, not shadcn's `ring-ring/50`, which falls below 3:1
    (`badge`, `button`, `field`, `input`);
  - the sheet's scrim is `bg-overlay/10`, not `bg-black/10`.
- **abc builds by hand only what shadcn lacks, from shadcn's pieces,** in `src/components/`:
  - **AppShell**: the frame of every signed-in page, from Sidebar, Badge, Avatar and the
    ThemeMenu. On the desktop, the rail (the logo and the navigation, each entry an icon over a
    mono label) and the header (the workspace's name, a pill per machine, the theme and the
    person's initials). On the phone, as the canvas's Home does: a top bar (the workspace's name,
    one pill counting the machines online, the theme and the initials), the page, and the same
    navigation as a tab bar at the bottom. One `nav` is both. The rail is as wide as its widest
    entry and never narrower than 80px, so no label overflows it. A notice about the whole app,
    such as a failed check of the session, sits in a slim warning band under the header.
  - **ThemeMenu**: DropdownMenu with a radio group of Light, Dark and System.
  - **TextField**: Field, FieldLabel, Input, FieldDescription and FieldError, with the label,
    description and error tied to the input by id; shadcn's Field leaves that to its user.
  - **TitledCard**: Card in a `section` named by its heading, with a Status beside the title.
  - **CopyableText**: a command or secret a person pastes elsewhere, in a mono well with a Copy
    Button. shadcn's only copy control is `code-block`, which brings the shiki highlighter for
    plain text. The value shows on one line, as it is pasted, never broken mid-word; one wider
    than its box scrolls inside it, and the Copy button stays in view beside it. Copy puts exactly
    the value on the clipboard, says "Copied" for two seconds through a live region, and selects
    the value instead where the clipboard is refused. The end-to-end checks fail on a code box
    whose value wraps, or that clips it instead of scrolling.
  - **Status**: Badge with a dot and a word for a state (`success`, `warning`, `danger`,
    `neutral`). The word is always there: colour is never the only signal.
  - **Logo**: abc's name in mono on an ink tile. shadcn has no logo, and Avatar is a person's.
  - Links are the router's `Link` with the primary colour; shadcn has no link component.
- **The rule is a test, not prose:** `web/src/shadcn.test.ts` fails when a hand-written component
  outside `src/components/ui/` has the name of a registry component, and when React Aria, which
  shadcn replaced, comes back. It reads the registry's list from `web/shadcn-registry.json`, which
  `cargo xtask pnpm exec shadcn search @shadcn --type ui --limit 1000 --json > web/shadcn-registry.json`
  refreshes.

Nothing a component does needs a looser CSP: Base UI sets styles through the DOM, which
`style-src 'self'` allows, and injects no `<style>` or `<script>` element for the components abc
uses. The end-to-end tests fail on any console error, a refused style or script included. Vite
never inlines files as `data:` URLs (`assetsInlineLimit: 0`), so the fonts load under
`font-src 'self'`.

## Accessibility

- **Contrast:** text is at least 4.5:1 against its background, and a control's edge and the focus
  ring at least 3:1, in both themes; the pairs are listed and tested above.
- **Touch targets:** below `md` every button, field, link, tab and menu item is at least 44 by
  44px: one rule in `styles.css` sets `--target-touch` as their least size. The end-to-end tests
  measure every control on every screen at 390px wide, in both themes.
- **Layout:** no navigation label overflows its entry or the rail, and forms keep a reading width
  (the claim card and the enrolment panel); the end-to-end tests measure both.
- **Focus:** keyboard focus is always visible: shadcn's ring in `--ring` around the control.
  Menus move focus with the arrow keys and give it back to their button when they close.
- **Colour is never the only signal:** every state has a word (Online, Offline, Reconnecting), and
  errors say what happened and what to do.
- **Names:** every control and region has an accessible name, and each component's test checks it.
- **Motion:** nothing moves on its own. Menus fade in when opened. A pending action says so in
  words ("Claiming…").
