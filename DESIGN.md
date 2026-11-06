# abc's design

How abc looks, in one place. The look comes from the design canvas the maintainer approved
(S0, November 2026): a calm, light, flat interface; Inter for what people read and JetBrains Mono
for labels and identifiers, with small uppercase mono labels as part of the look.

The tokens below are the only source of the look. `web/src/styles.css` declares them in Tailwind's
`@theme`, which turns each into CSS variables and utilities (`bg-surface`, `text-ink-muted`,
`rounded-card`, ...). `web/src/design.test.ts` reads this file and the stylesheet and fails when
the two differ, or when a text pair listed under [Contrast](#contrast) falls below 4.5:1. Change a
token here and in `@theme` together. Components use tokens only: no colour, size or radius is
written anywhere else.

`@theme` resets Tailwind's own colours, fonts, text sizes, radii and shadows (`--color-*: initial`
and so on), so a utility outside this list does not exist. Tailwind's spacing step and breakpoints
stay, and are listed here as tokens too.

## Colour

| Token | Value | Purpose |
| --- | --- | --- |
| `--color-ground` | `#F7F7F5` | The page behind everything. |
| `--color-surface` | `#FFFFFF` | Cards, the header, the rail and the tab bar. |
| `--color-sunken` | `#FBFBFA` | Wells inside a surface: text fields and commands. |
| `--color-ink` | `#1C1C1A` | Primary text, and the logo tile. |
| `--color-ink-muted` | `#5F5E59` | Secondary text and inactive navigation. |
| `--color-ink-subtle` | `#6E6D68` | Mono labels and metadata, on the ground, a surface or a well only. |
| `--color-rule` | `#E6E5E1` | Dividers and card borders: decoration, never a control's only edge. |
| `--color-rule-strong` | `#D4D2CC` | A secondary button's border, beside its label. |
| `--color-control` | `#878680` | A text field's border: 3:1 against every surface, as controls need. |
| `--color-accent` | `#3451D1` | Primary actions, links, the current page and the focus ring. |
| `--color-accent-strong` | `#2639A3` | A primary action while hovered or pressed. |
| `--color-accent-tint` | `#EBEEFA` | The current page's background, and the focus halo of fields. |
| `--color-success` | `#1F7A3A` | Online and healthy: text, dots and borders. |
| `--color-success-tint` | `#E9F2EB` | The background of a success pill. |
| `--color-warning` | `#B35400` | Reconnecting and stale: dots and borders, text on the ground or a surface. |
| `--color-warning-ink` | `#934500` | Warning text on the warning tint, where `--color-warning` is too light. |
| `--color-warning-tint` | `#F7EEE6` | The background of a warning pill or banner. |
| `--color-danger` | `#B3261E` | Errors, refusals and revoked machines. |
| `--color-danger-tint` | `#F7E9E8` | The background of a danger pill or message. |
| `--color-neutral-tint` | `#EEEDE9` | The background of a neutral pill, such as offline. |

The canvas draws text fields with `--color-rule`; that edge is 1.2:1 against white, so fields use
`--color-control` instead. The canvas also sets warning labels in `--color-warning` on the warning
tint (4.4:1); they use `--color-warning-ink`.

## Type

| Token | Value | Purpose |
| --- | --- | --- |
| `--font-sans` | `"Inter Variable", ui-sans-serif, system-ui, sans-serif` | Everything people read. |
| `--font-mono` | `"JetBrains Mono Variable", ui-monospace, monospace` | Labels, identifiers, commands and codes. |
| `--text-label` | `11px` | Small uppercase mono labels (`font-mono uppercase tracking-label`). |
| `--text-label--line-height` | `16px` | Their line height. |
| `--text-sm` | `12px` | Metadata, pills and secondary lines. |
| `--text-sm--line-height` | `16px` | Its line height. |
| `--text-base` | `13px` | Body text, buttons and fields. |
| `--text-base--line-height` | `20px` | Its line height. |
| `--text-md` | `15px` | Card titles and the workspace name. |
| `--text-md--line-height` | `22px` | Its line height. |
| `--text-xl` | `22px` | Page titles. |
| `--text-xl--line-height` | `28px` | Its line height. |
| `--tracking-label` | `0.06em` | The letter spacing of uppercase labels. |

The fonts are self-hosted from the `@fontsource-variable/inter` and
`@fontsource-variable/jetbrains-mono` packages, since the app's CSP is `default-src 'self'`. Weights
are 400 for text, 500 for labels and buttons, and 600 for titles.

## Space, radius and elevation

| Token | Value | Purpose |
| --- | --- | --- |
| `--spacing` | `0.25rem` | The spacing step: `p-4` is 16px. Layouts use steps of 1, 2, 3, 4, 5 and 6. |
| `--radius-chip` | `3px` | Chips and inline codes. |
| `--radius-control` | `6px` | Buttons, pills and text fields. |
| `--radius-nav` | `8px` | Navigation items and the logo tile. |
| `--radius-card` | `12px` | Cards. |
| `--shadow-halo` | `0 0 0 3px var(--color-accent-tint)` | The halo around a focused text field, beside its accent border. |
| `--breakpoint-md` | `48rem` | Below it the phone layout (top bar and tab bar), from it the desktop one (rail and header). |

abc is flat: surfaces are set apart by rules, never shadows. The halo is the one elevation.

## Contrast

Every pair of text and background the components use, each at least 4.5:1 (WCAG 2.2 AA for normal
text). The test computes each ratio from the tokens above.

| Text | Background |
| --- | --- |
| `--color-ink` | `--color-ground` |
| `--color-ink` | `--color-surface` |
| `--color-ink` | `--color-sunken` |
| `--color-ink-muted` | `--color-ground` |
| `--color-ink-muted` | `--color-surface` |
| `--color-ink-muted` | `--color-neutral-tint` |
| `--color-ink-subtle` | `--color-ground` |
| `--color-ink-subtle` | `--color-surface` |
| `--color-ink-subtle` | `--color-sunken` |
| `--color-accent` | `--color-surface` |
| `--color-accent` | `--color-accent-tint` |
| `--color-surface` | `--color-accent` |
| `--color-surface` | `--color-accent-strong` |
| `--color-surface` | `--color-ink` |
| `--color-success` | `--color-success-tint` |
| `--color-warning-ink` | `--color-warning-tint` |
| `--color-danger` | `--color-surface` |
| `--color-danger` | `--color-danger-tint` |

## Components

abc owns a small set of components in `web/src/ui/`. Each wraps a React Aria Components primitive,
which brings the behaviour, keyboard handling and ARIA, and adds abc's look from the tokens alone.
Screens build from these and from plain layout, never from raw React Aria with their own styling.

- **Button**: a React Aria `Button`, `primary` (accent fill) or `secondary` (surface with a rule
  border). While `isPending` it ignores presses, and React Aria announces the pending state.
- **TextField**: a React Aria `TextField` with its `Label`, `Input`, a description and a
  `FieldError`; the error is tied to the input, so a screen reader reads it with the field.
- **StatusPill**: a dot and a word for a state (`success`, `warning`, `danger`, `neutral`). The
  word is always there: colour is never the only signal.
- **Card**: a surface with a rule border and the card radius, as a `section` named by its heading,
  with its state, if any, as a StatusPill beside the title.
- **Link**: a React Aria `Link` in the accent colour, underlined, navigating through the router.
- **Logo**: abc's name in mono on an ink tile, read as the word it shows.
- **AppShell**: the frame of every signed-in page. On the desktop: the rail (the logo tile and
  navigation, each entry an icon over a mono label) and the header (the workspace's name, a pill
  per machine, the person's initials). On the phone, as the canvas's Home does: a top bar (the
  workspace's name, one pill counting the machines online, and the initials), the page, and the
  same navigation as a tab bar at the bottom. One `nav` is both the rail and the tab bar.

React Aria adds a `<style>` for pressable elements that the CSP (`style-src 'self'`) blocks, so
`web/src/styles.css` carries that rule and `web/index.html` holds the element React Aria looks
for to know it is there. Vite never inlines files as `data:` URLs (`assetsInlineLimit: 0`), so
the fonts load under `font-src 'self'`.

## Accessibility

- **Contrast:** text is at least 4.5:1 against its background; the pairs are listed and tested
  above. A control's edge, and the focus ring, are at least 3:1.
- **Touch targets:** every control is at least 44 by 44 px on a touch screen (`pointer-coarse:`),
  and at least 24 px elsewhere.
- **Focus:** keyboard focus is always visible: a 2px `--color-accent` outline, 2px off the control
  (React Aria's `data-focus-visible`), and a field's accent border with the halo.
- **Colour is never the only signal:** every state has a word (Online, Offline, Reconnecting), and
  errors say what happened and what to do.
- **Names:** every control and region has an accessible name, and each component's test checks it.
- **Motion:** nothing moves on its own. A pending action says so in words ("Claiming…") and is
  announced by React Aria.
