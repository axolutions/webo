# webo design system

The single source of truth for how the webo UI looks and behaves. Everything
here is extracted from the living CSS in `web/index.html` — when the two
disagree, fix one of them in the same PR.

**The rule that matters most:** the panel is built from a small, closed
vocabulary of components. A new feature reuses this vocabulary; it does not
invent parallel classes. If a genuinely new component is needed, add it here
in the same PR that introduces it.

New UI ships in three layers, and all three must land together — this has
bitten us before (a card shipped with no load call, no i18n keys, and no CSS,
each invisible from the API side):

1. the markup + the JS that loads its data,
2. every user-facing string in **both** i18n dictionaries (`en` and `pt`),
3. CSS for every class the markup uses (the `i18n_tests` in `server.rs`
   catches missing strings; nothing catches missing CSS yet — check the
   rendered screen).

## Feel

Dark-first glassmorphism over a deep navy gradient. Cards are translucent
white layers (`backdrop-filter: blur`) with soft strokes and long soft
shadows. One blue→violet accent gradient carries all "primary" moments.
Density is medium: 14px base type, 20–22px card padding, 16px grid gaps.
Corners are round everywhere (9–16px; pills at 999px). Motion is minimal —
a card hover lift, a chevron rotate, a spinner; nothing else animates.

## Tokens

Defined on `:root` (dark, the default) and overridden on
`[data-theme="light"]`. **Never hardcode a color in a component — always go
through a token.** The only exceptions currently in the file are the danger
soft backgrounds (`rgba(229,72,77,…)`) which pair with `--danger`.

| Token | Dark | Light | Use |
|---|---|---|---|
| `--accent` | `#4f8bff` | same | links, active states, focus rings, primary gradient start |
| `--accent-2` | `#7c5cff` | same | primary gradient end (never alone) |
| `--accent-soft` | `rgba(79,139,255,.16)` | `rgba(31,107,255,.12)` | soft blue chip/selection backgrounds |
| `--page` | navy radial+linear gradient | pale blue gradient | `body` background only |
| `--surface` | `rgba(255,255,255,.045)` | `rgba(255,255,255,.62)` | subtle fill: hover rows, inset boxes |
| `--surface-2` | `rgba(255,255,255,.075)` | `rgba(255,255,255,.82)` | inputs, secondary buttons, chips |
| `--surface-3` | `rgba(255,255,255,.11)` | `rgba(15,32,68,.06)` | track backgrounds, neutral pills, seg control |
| `--card` | white 6%→3% vertical gradient | white 90%→72% | card backgrounds (with `--stroke` border + `--sh` shadow + blur) |
| `--stroke` | `rgba(255,255,255,.085)` | `rgba(15,32,68,.09)` | default borders and row dividers |
| `--stroke-strong` | `rgba(255,255,255,.2)` | `rgba(15,32,68,.2)` | hover borders, tooltip borders, modal border |
| `--text` | `#eaf0fd` | `#0f1c33` | primary text |
| `--text-2` | `#a2b4d4` | `#4c6183` | secondary text, most body copy |
| `--text-3` | `#6f83a5` | `#7c8dab` | labels, captions, placeholders, icons at rest |
| `--ok` | `#3fca7c` | same | "live/healthy" — dots, done steps |
| `--ok-soft` | `rgba(63,202,124,.14)` | same | soft green backgrounds |
| `--danger` | `#e5484d` | same | errors, destructive actions, write mode |
| `--sh` | inset highlight + long soft drop | lighter pair | card shadow (always with `--card`) |
| `--mono` | IBM Plex Mono stack | same | see typography |
| `--spark-ring` | `#0a1226` | `#eef3fc` | halo behind sparkline endpoint dots |

Semantic colors are reserved: green = alive/success, red = error/destructive,
amber (`rgba(234,179,8,…)`, used inline in `.warn-strip` and the browser
error badge) = warning/attention. Never use them as decoration or as a
"series 2" chart color.

## Typography

Two faces, loaded from Google Fonts:

- **IBM Plex Sans** (400/500/600/700) — everything by default.
- **IBM Plex Mono** (400/500), via `var(--mono)` — anything machine-flavored:
  slugs, hashes, env keys/values, SQL, logs, table headers, badges like
  `RD-007661`.

Scale (there is no h1–h6; hierarchy is size + weight + color):

| Role | Spec |
|---|---|
| Hero number | 36px / 700 / `-0.02em` / `tabular-nums` (30px in tiles and on mobile) |
| Hero unit | 15–17px / 600 / `--text-3` |
| Brand / page title | 20px / 700 / `-0.02em` |
| Body / rows | 13–13.5px / 400–600 |
| Base | 14px on `body` |
| Section label | 11px / 600 / `letter-spacing: .09em` / UPPERCASE / `--text-3` (class `.label`) |
| Badges & pills | 10.5px / 600 |
| Log/mono lines | 11.5–12.5px mono |

Numbers that update or align vertically always get
`font-variant-numeric: tabular-nums`.

## Radii, spacing, layout

- Radii: 15px cards · 16px modal · 11–12px buttons/inputs/icons ·
  9–10px small buttons/chips · 999px pills, dots and bar tracks.
- Page: `.wrap` — max-width 1240px, `22px 24px 40px` padding.
- Grids: 16px gap. Overview `main` is 3 columns; projects grid 3;
  detail tiles 4; detail body `.detail-cols` is `1.25fr 1fr`.
- Card padding: `20px 22px`. Card header (`.dcard-title`): `14px 20px`
  with bottom border. Rows inside cards: `12–13px 20px`.
- Breakpoints: `900px` (3→2 columns, detail collapses to 1 column, process
  table drops time/disk columns) and `620px` (everything 1 column, hero
  30px, process rows wrap to two lines).
- Wide content (tables) scrolls inside `.grid-wrap { overflow-x: auto }` —
  the page body itself must never scroll horizontally.

## Component vocabulary

Reuse these. Class names are the API.

### Cards
- `.stat-card` — overview stat: `.card-head` (`.label` + side info) →
  `.hero-row` (`.hero` + `.hero-unit` + `.hero-side`) → visualization →
  `.sub` caption.
- `.tile` — small stat on the project detail (`.big` + `.unit`).
- `.dcard` — detail section card: `.dcard-title` header
  (`.label` left, `.sub` right) + content rows. **Every new detail section
  is a `.dcard`.**
- `.proj-card` — clickable project card (hover: lift 2px + stronger border).
- `.danger-card` — `.dcard` variant with red border for destructive areas.

### Rows (all: flex, gap ~13px, `padding: 12–13px 20px`, top border, first
child borderless)
- `.drow` generic · `.res-row` resource · `.kv-row` env var (mono key/value)
  · `.wrow` wizard repo (`.pick` clickable, `.sel` selected with inset accent
  bar, `.dim` 45% opacity) · `.issue-row` error group (with `.issue-count`
  mono counter and expandable `.issue-events`) · `.log-line`
  (`.log-ts`/`.log-src`/`.log-txt`, `.log-err` paints text red) ·
  `.proc-grid` process table row.

### Buttons
- `.btn-primary` — 40px, accent gradient, white text, glow shadow. One
  primary action per view.
- `.btn-ghost` — 40px, `--surface-2` + stroke. Secondary.
- `.btn-sm` — 32px compact version of ghost (row-level actions).
- `.btn-danger` — red-tinted outline; `.solid` variant fills red (final
  confirmation only).
- `.icon-btn` — 38×38 icon-only square.
- Disabled = `opacity: .4`, shadow off. Focus =
  `outline: 2px solid var(--accent); outline-offset: 2px` — every
  interactive element must keep a visible focus state.

### Badges & chips (pills, 10.5px/600 unless noted)
- `.pill` / `.src-badge` / `.procs-badge` — neutral on `--surface-3`.
- `.role-badge` — colored per role, soft background of the same hue.
- `.err-badge` — red count of open errors.
- `.tag-managed` (accent) / `.tag-missing` (red) — env var state.
- `.mono-badge` — mono accent chip (host id).
- `.chip` — 30px header info chip with `.dot` status (green glow = up,
  `.off` red = down).
- `.step-chip` — wizard steps: `.on` accent, `.done` green.

### Inputs
- `.search-box` — 38px, focus ring `0 0 0 4px var(--accent-soft)`.
- `.dom-input` / `.del-input` — 40px mono inputs; same accent focus ring
  (del-input rings red).
- `.sql-box` — mono textarea, vertical resize.
- `.write-toggle` — checkbox with `accent-color: var(--danger)`; write mode
  is always visually red.

### Data displays
- `.bar-track`/`.bar-fill` — 8px progress, accent gradient fill, optional
  `.bar-cap` limit marker; `.mini-track`/`.mini-fill` — 5px inline version.
- `.spark` — 56px-tall interactive SVG sparkline; hover shows `.spark-tip`
  tooltip + `.spark-cursor` crosshair (built by `interactiveSpark()` in JS).
- `table.data` — mono cells, sticky uppercase mono header, wrapped in
  `.grid-wrap`.
- `.snippet-box` — mono copy-paste box, capped height, scrolls.

### Overlays
- `.modal-overlay` (fixed, blurred dark) + `.modal-card` (460px,
  `--stroke-strong` border). Destructive modals: checkboxes with red accent,
  type-to-confirm `.del-input`, `.btn-ghost` cancel + `.btn-danger.solid`
  confirm that stays disabled until the typed name matches.
- `.warn-strip` — inline amber warning band.
- `.spin` — 16px accent spinner (`wspin` 0.9s).

## Writing & i18n

- Every user-facing string goes through `t("key")` and exists in **both**
  dictionaries (`en` default, `pt`). CI enforces this
  (`server::i18n_tests`).
- Voice: lowercase, human, concrete — "no ar há 1 d 13 h",
  "quase dormindo · últimas 24 h", "nenhum erro até agora — esse é o
  desfecho bom". Say what happens, not the mechanism.
- Labels on cards are short nouns; buttons are verbs ("copiar",
  "conectar", "resolver").
- Dates/numbers format through the active locale (`toLocaleString(LOCALE)`).

## Theming rules

- Dark is the default (`:root`); light is an explicit override on
  `[data-theme="light"]`, toggled by the header button and persisted in
  `localStorage("webo-theme")`; with no stored choice the OS preference wins.
- A component must never define a color outside the token set — that is
  what keeps both themes working without per-component overrides.

## Known debt (do not copy these patterns)

- `.log-line` uses a hardcoded `rgba(255,255,255,0.03)` border that ignores
  the light theme.
- Danger soft backgrounds repeat `rgba(229,72,77,…)` inline in five places —
  candidates for a `--danger-soft` token.
- The Variables card shows a bare "…" for ~6s while loading; loading states
  have no skeleton pattern yet.
