[← Back to README](../README.md)

# Gateway panel (WebUI)

The gateway panel is the gateway's own management page: the TypeScript sources in `webui/` are built by Vite into a single `webui/dist/index.html`, embedded at compile time by `src/http/static.rs`, and served at `GET /`. This page covers the interface itself; routes, fields and error codes are in [API endpoints](api.md).

## Files

| File | Role |
| --- | --- |
| `webui/index.html` | Page shell: side rail, top bar, five page containers, dialog; the no-script fallback text also lives here |
| `webui/src/main.ts` | Boot order, navigation, frame chrome, per-page render isolation |
| `webui/src/text.ts` | Every visible string, paired Chinese and English, in one place |
| `webui/src/state.ts` | Language detection, tolerant reads/writes of the three storage keys, probe-model default |
| `webui/src/api.ts` | Same-origin requests, gateway-key channel, one prompt for concurrent 401s with one retry each, failure copy |
| `webui/src/data.ts` | Defensive JSON parsing, session-wide catalogue cache, endpoint wrappers |
| `webui/src/store.ts` | Shared view state and write actions (probe, switch, Orb, refresh) |
| `webui/src/format.ts` | Dates, money, percentages, enum-to-copy mappings |
| `webui/src/ui.ts`, `dom.ts`, `dialog.ts` | Shared loading/empty/error blocks, element builder, the single modal implementation |
| `webui/src/pages/*.ts` | One renderer per page |
| `webui/src/selfcheck.ts` | Dev-only assertion script for the merge, billing and accessible-name branches; not part of the bundle |
| `webui/src/theme.css` | All styles and design tokens |

## Boot order and render isolation

The order is fixed: the frame renders first (rail, top bar, navigation, language switch, toast layer, five page containers), then language detection and storage reads, then each page renders and loads its own data, and only then the clock, visibility and hash listeners attach.

The navigation is plain buttons, not a router library: a valid hash shows its page at load, `hashchange` follows the back and forward buttons without reloading, and each entry is marked with `aria-current="page"`. The account entry carries a count badge that stays absent until the account list has loaded, so a failed load never reads as "0 accounts". At 780 px wide and below the rail turns into a header above the content and the navigation wraps; every entry keeps a 44 px target. The metrics grid drops to two columns at 960 px and the route table stacks its columns there too. Diagnostics runs its six checks once when the panel loads; switching back to the page does not run them again, and "Check again" is the only re-run.

Each page render is wrapped in its own guard. When one page throws, that page shows an error surface (what happened, a plain-language reason, retry, reload) while the other pages, the navigation and the language switch stay usable. The error surface has its own fallback: if the DOM helpers used to build it also fail, the container still gets a single line of text. Exceptions inside async updates put only that view into its error state. No module-level code reads local storage, so a browser that blocks site data still renders the whole panel.

## Design direction

**Reader and setting.** The reader runs this gateway. They open a one-off gateway panel on localhost or a LAN address to check whether accounts still work, whether tokens expired, how much quota is left, and whether the model catalogue moved. The task is verification, not operations, so the layout is organised around density and scan speed rather than dashboard-style metrics.

**Colour (two core colours plus one accent).** Paper `#f7f4ec` and ink `#1d1c19` carry every background and text level. The accent is terracotta `#9c3a1a`, and it is the only colour that marks attention: a primary action (probe all, add account, save an Orb link, finish a login, refresh the catalogue, run the checks), the left edge of banners and of the active account, the selected model row, the login panel edge, the top rule of each protocol strip, the balance fill, the avatar wash, the diamond in front of a key-required route, the focus ring and the spinner arc. Links use the darker variant `#7f2d12` so body-sized link text clears 4.5:1 on paper. The page is almost entirely text and hairlines, so one signal colour is enough; spreading it further would stop marking a primary action. Green, amber and red exist only as functional status colours on tags and notices; they are never decoration.

**Contrast.** Body ink on paper is about 15.5:1; secondary text `#59554c` about 6.7:1; the accent as text about 6.3:1; light text on the dark rail about 15:1 and secondary rail text about 9.7:1. All clear WCAG AA 4.5:1, with room above the 3:1 floor for large text and focus rings. Control and container boundaries use `#8a8375`, 3.4:1 or better on both paper and card, so inputs, selects, secondary buttons and card edges stay visible without the accent competing.

**Type and hierarchy.** Headings use a serif stack (Iowan Old Style / Palatino / Georgia / Noto Serif), body text the system sans stack, and machine values (model ids, paths, amounts, times, storage keys) a monospace stack. Each stack has one job. Hierarchy comes from size, weight and space, not shadow. Serif appears at page and section level, on the four metric values and on the dialog title; card titles, row labels and every control return to sans so the whole page does not read as a masthead. Small labels and column heads are uppercase and letter-spaced at 0.72 to 0.78 rem; that is the one repeated typographic device, and it is what makes a ruled list still scan as a table.

**Rules instead of stacked cards.** Accounts, models, checks and routes are lists of rows separated by hairlines, not one raised card per item. Lists run to twenty or thirty entries, where repeated padding and borders inside cards slow scanning down. Accounts carry many fields, so they sit in a thin-bordered container, but the interior is still a ruled grid with no nested radii or shadow.

**Radius scale.** Three values: 4 px (tags, small squares, icon boxes), 8 px (buttons, inputs, list containers, banners), 12 px (account containers and dialogs), plus a 999 px pill for status tags. No component picks a fourth value; the status dot, the avatar circle and the spinner stay circles as shapes, not as a radius choice.

**Rail and navigation.** The rail is the one dark surface, so the five navigation entries stay visible while the page area changes. Entries are numbered `01` to `05` in a monospace column, decorative and hidden from assistive tech: the numbers give the eye a fixed anchor when the current page is marked by an underline plus background, and they cost nothing when ignored. The rail also carries the service state at the top, because "is the gateway answering at all" is the first question this page answers, and the two factual notes at the bottom.

**Spacing scale.** 4 / 8 / 12 / 16 / 24 / 32 / 48. Rows use 16 px padding and separate with a hairline instead of stacking margins.

**State is never colour alone.** Quota has six states (available, exhausted, unmetered, restricted, billing unavailable, token invalid) and probes five; each pairs an icon (check, cross, exclamation, dot) with a text label, so a greyscale screenshot still distinguishes them. The balance bar prints its percentage and exposes a `progressbar` role with a value.

**Motion.** Two places only: the toast expanding over 160 ms and the loading spinner. Under the system reduced-motion preference both drop to near zero and the spinner becomes a static pattern; in-progress states still read from the text ("Refreshing…"), never from animation alone.

**Template devices left out.** No gradients, glass, glow or coloured shadows; no headline-plus-equal-card-grid first screen; no fake terminal chrome (the client snippets are real, copyable plain text); no logo wall, statistic pile or invented compliance badge. Icons are one set of 24 px inline vectors with a consistent stroke, decorative only and hidden from assistive tech.

## The five pages

| Page | Hash | Contents |
| --- | --- | --- |
| Accounts | `#accounts` | Four header controls (probe model, refresh account status, refresh Orb balance, probe all), a cost banner, four metrics, the account list, add-account and the login panel; accounts are added through the paste login and switched between, never deleted or renamed, because the backend has no such endpoints |
| Model list | `#models` | Catalogue refresh, an explainer banner, model rows (display name, id, provider, description, default and supported levels, context length, use-for-probe); entries that only the upstream `data[]` list carries come in as read-only rows marked "Upstream list only" |
| Diagnostics | `#health` | Six read-only checks in a fixed order, per-row state, a summary with finish time, run again |
| Routes | `#endpoints` | Three protocol entries and six static route groups (page and liveness, models and protocols, compatibility, accounts and quota, catalogue and identity, login), each row with method and whether a gateway key is needed; the page sends no requests |
| Clients | `#integration` | Base-address notes and four copy-ready snippets (Codex, Claude Code, OpenCode, a shell request) |

The first three navigation entries form "Runtime", the last two "Developer tools". The current page is marked with `aria-current="page"` and an underline, not colour alone.

## Language and storage

Detection reads `zed_lang` first and accepts only the exact values `zh` or `en`; otherwise `navigator.language` decides, with a `zh` prefix meaning Chinese. Switching language writes the key and redraws the frame and all five pages in place, so the reader stays on the same page and the address fragment does not change.

Only three keys are used: `zed_lang`, `zed_api_key`, `zed_probe_model`. Every read and write goes through the tolerant wrappers in `state.ts`: a failed read counts as absent, a failed write is ignored while the in-memory value keeps working. With site data blocked, language falls back to the browser but a language picked in the top bar still applies to the current session (the pages are redrawn in place, no reload), the key lasts for the session, and the probe model returns to `gpt-5.6-luna`; all five pages and the navigation still render. Unknown historical keys are neither read nor cleaned up.

## Gateway key and 401s

Protected requests carry `Authorization: Bearer <key>`; an empty key means no header. Any 401 opens an in-page dialog asking for the key: one field, password type, show/hide, Enter submits, Esc cancels, empty is refused. Only one dialog may exist; other 401s wait for the same answer and each retry once. A second 401 fails the request instead of asking again, and the rejected key is remembered so the dialog does not reappear for it. Cancelling fails every waiting request with its original 401, and each view shows its own error state.

## Error presentation

Every data view fails as "what happened, a plain reason, retry". Reasons map from status codes: 400 request rejected, 401 gateway key, 403 refused, 404 account missing, 409 login running, 410 login expired, 413 too large, 429 too many requests, 5xx and network failures as service or upstream temporarily failing. Raw JSON, stacks, request headers and internal addresses never reach the interface; short machine codes (underscore identifiers) go to the browser console only.

Views that keep earlier values say so: "Showing data from an earlier successful load (time)". A failed write leaves its button usable so it can be retried. When a write gets no answer at all, the toast says the result is unknown and points at "Refresh account status", which is the only way to learn whether the backend applied it.

Login failures get one line per status class: 409 means no callback is waiting, 410 means the attempt expired, 500 means the account did not save, and 400 names the callback problem (too long, missing user id or access token, not a callback, decryption failed, credential not JSON) without echoing the pasted URL. A rejected Orb link keeps the input and spells out the accepted shape: https, the exact `portal.withorb.com` host, and a non-empty token parameter. The account card shows the cooldown countdown next to the probe result, and the probe budget starts from the catalogue estimate and switches to the limit the probe response reports once a probe has run.

## Client snippets

Snippets are built from runtime values: the base address comes from the page's protocol, host and port (falling back to 8001 when there is no port); the example model is the selected probe model when the catalogue lists it, otherwise the first entry, and `<model-id>` with a "replace it" note when no catalogue arrived. The effort example is `xhigh`, with a pointer to `/v1/models` for the available levels. Keys are always placeholders and are never read from storage. Each snippet is a focusable monospace block, so it can be selected and copied by keyboard when the clipboard is blocked; Ctrl/Cmd+A inside a focused block selects that snippet only, not the whole page.

## Build and compatibility

- `npm ci && npx tsc --noEmit && npm run build` produces the single `webui/dist/index.html` with no external origins; fonts come from system stacks.
- The bundle contains each of the five page anchors (`page-accounts`, `page-models`, `page-health`, `page-endpoints`, `page-integration`) exactly once; the tests in `src/http/static.rs` look for them. Page containers exist in the HTML from the start and the script renders into them without a router library.
- `build.rs` fails the build when the bundle is missing and tells you to build the interface first.

## Acceptance material

Page acceptance runs against a fake gateway (local, fixed fixtures, no real credentials) in a headless browser: Chinese and English screenshots of all five pages at desktop and phone widths, no horizontal overflow at narrow widths, 44 px target checks, a keyboard walk, Esc closing dialogs and returning focus, static waiting under reduced motion, storage-blocked degradation, single-page render-fault isolation, and the static fallback with scripts disabled. The run also covers state screenshots, focus rings, a 320 px page and 200 % zoom, an axe-core scan per page and a hand-checked contrast and focus-order log.

The full set, including logs, lives in `F:\Projects\tmp\.scratch\zed-proxy\webui-check\rust-0.3.0-redesign\` and is not committed. Four captures from that set are committed for the README, straight copies with fake data and no real account: `docs/assets/webui-accounts.en.png`, `webui-accounts.zh.png`, `webui-health.en.png`, `webui-health.zh.png` (wide, 1440 px).
