# tablica: overall structure and family dashboard

Date: 2026-11-25
Status: approved

**Name:** `tablica` (Polish for "board"). It's the crate name, the binary name
(`tablica serve`, `tablica capture …`) and the image name.

## Intent

This is a small, self-hosted backend for two TRMNL 7.5" (OG) DIY kits. It serves two
fixed dashboards: **family** (calendar, kids' school timetable, weather) and **homelab**
(to be designed later). It runs as one Docker container on the NAS.

**Why custom:** the requirements are specific (Librus timetables, a schedule that changes
during the day), and nobody needs to customize anything at runtime. The existing BYOS
servers (Terminus, LaraPaper, BYOS Next.js, etc.) are general-purpose platforms with
plugin systems, admin UIs and databases. Here the server is configured once and left
alone. Writing it ourselves in Rust gives us one small binary that we control completely,
with no Node, Ruby or PHP runtime to maintain.

This document covers:
- the overall structure of the server, shared by both dashboards
- the family dashboard in full detail

The homelab dashboard only gets a placeholder slot here.

## Goals

- The device firmware gets everything it needs from this server: setup, display, log.
- The family dashboard shows:
  - the coming week of the family's iCloud calendar
  - each of the two kids' school timetables from Librus
  - current weather and the forecast
- It switches between "today" and "next school day" at a set time each afternoon.
- Images are clean 2-bit grayscale, with 1-bit as a fallback setting for each device.
- Failures don't show up on the screen. If a data source fails, the dashboard keeps
  showing the last good data with a small "as of" note.
- Everything is configured through one config file plus secrets in environment
  variables, and it runs as one container.
- During design work, a template edit shows up in a preview within about a second.

## Language

- **All text on the dashboards is in Polish:** headings, dates, weekday and month
  names, weather descriptions, notes and placeholders. Librus data (subjects, flags)
  is already Polish and passes through unchanged.
- **Everything for developers is in English:** code, identifiers, comments, log
  messages, config keys, commit messages and documentation (including this document).
- Examples of the on-screen wording (the builder may adjust the exact phrasing):

  | Concept | On screen |
  |---|---|
  | Date header | "Środa, 25 listopada" (weekday capitalised, month in the genitive) |
  | Today / tomorrow | "Dziś", "Jutro" |
  | Weekday short | "pon.", "wt.", "śr.", "czw.", "pt.", "sob.", "niedz." |
  | Calendar heading | "TEN TYDZIEŃ" |
  | All-day event | "cały dzień" |
  | Overflow | "+3 więcej" |
  | High / low | "maks. 6° min. 1°" |
  | Precipitation note | "deszcz 30% od 15:00", "śnieg 40% od 9:00" |
  | Conditions | "Bezchmurnie", "Częściowe zachmurzenie", "Pochmurno", "Mgła", "Deszcz", "Śnieg", "Burza" |
  | Stale note | "stan na pon. 07:30" |
  | Source never loaded | "Kalendarz niedostępny", "Plan lekcji niedostępny", "Pogoda niedostępna" |
  | No school | "Brak lekcji" |
  | Homelab placeholder | "Homelab: wkrótce" |

- All strings live in one place, a Polish string table or module, rather than being
  scattered through templates. Proper translation infrastructure isn't needed: there's
  one language and it's fixed. Month names need the genitive form ("listopada", not
  "listopad"), so generic locale formatting must be checked or replaced with our own
  tables.

## Non-goals

- Any user interface for managing devices, dashboards or plugins.
- A database. A small cache on disk is enough.
- Support for devices other than the two OG kits, or for TRMNL's cloud and plugin system.
- Writing anything back to Librus or iCloud.
- Firmware updates delivered through this server (`update_firmware` is always false).

## Anti-goals (would count as failure even if it technically works)

- **The dashboard shows an error, a blank screen or a stack trace** because Librus or
  iCloud had a bad moment.
- **The server polls Librus often enough to look like scraping.** Its terms of service
  frown on systematic bulk downloading, and an account block would break the dashboard.
- **The container needs regular attention:** dependency churn, a browser runtime to keep
  updated, or manual restarts.
- **Text is blurry or broken on the panel.**

## Hardware facts

- 2× TRMNL 7.5" (OG) DIY kit: 800×480 px, ESP32-C3, a panel about 163 mm wide (≈125 dpi).
- TRMNL's device model list shows OG support for:
  - `og_png`: 1-bit, 2 colors
  - `og_plus`: 2-bit, 4 grays
  - 4-bit (16 grays) is not supported. Only TRMNL X has it.
- The image must be a PNG, JPEG or 1-bit BMP of at most **90,000 bytes**. The firmware
  chooses the decoder from the `Content-Type` header, not from the URL.
- Recent firmware (v1.8.10 and later) fixed 2-bit gray rendering on the OG. We target
  current firmware (v1.8.19 at the time of writing).

## Approaches considered

### Rendering (the main decision)

| Approach | Trade-offs | Verdict |
|---|---|---|
| **HTML + headless Chromium** (what most BYOS servers do) | CSS layout, so design iteration is easy. But it adds 300+ MB to the image, several hundred MB of RAM on the NAS, and a browser runtime to keep updated, which is exactly the kind of dependency we want to avoid. Anti-aliased text has to be converted to grays afterwards. | Rejected |
| **SVG templates + resvg** | A tiny binary that can draw text without anti-aliasing. But SVG has no layout engine: every position is calculated by hand, and nothing wraps or truncates text automatically. The test render showed summary lines running past the column borders. | Rejected |
| **HTML/CSS templates + Takumi** (Rust, no browser) | Flexbox, grid, wrapping and ellipsis all come from CSS. Renders in about 7 ms, and there's no browser. It can't turn off text anti-aliasing, but after converting to 2-bit the text came out clean in the test. The crate releases very often and has broken its API within 2.x before. | **Chosen** |

How we tested it: we rendered the actual layout through both Takumi and resvg in Rust,
then compared the 1-bit, 2-bit and 4-bit output. We also tried four fonts (Inter,
Atkinson Hyperlegible Next, IBM Plex Sans, Noto Sans). **Inter at 2-bit** was the
clear favourite.

### Templating

| Option | Trade-offs | Verdict |
|---|---|---|
| **MiniJinja** | Templates are compiled at runtime and reload automatically while you edit, which matters because layout work is many small tweaks. The production build can still embed the templates in the binary. | **Chosen** |
| Askama | Templates are checked when the code compiles, but every tweak needs a rebuild. That's a weak benefit with only two templates. | Rejected |
| Tera | Similar to MiniJinja but with more dependencies. | Rejected |

### Building on an existing BYOS server

We rejected this. See Intent. The device protocol is three small endpoints. The real
work is the data integrations (Librus) and the layout, and no existing project gives us
either of those.

## Architecture

```
                 ┌───────────────────────────────── tablica (Rust, one binary) ───────────────────────────────┐
 TRMNL OG ──────▶│ Device API (axum)          Dashboards                    Renderer                          │
 (×2)            │  GET  /api/setup     ──▶  family::build_view(now,  ──▶  MiniJinja HTML ─▶ Takumi ─▶ RGBA   │
                 │  GET  /api/display          cache) -> ViewModel          ─▶ quantize (2-bit|1-bit) ─▶ PNG  │
                 │  POST /api/log            homelab (later)                                                  │
                 │  GET  /images/{hash}.png                                                                   │
                 │  GET  /preview/{dashboard}.png?at=…&depth=…                                                │
                 │                                                                                            │
                 │ Fetchers (tokio tasks on their own schedules) ──▶ Cache (memory + JSON files in /data)     │
                 │  icloud_ics · librus (×2 accounts) · open_meteo                                            │
                 └────────────────────────────────────────────────────────────────────────────────────────────┘
```

- **Language and runtime:** Rust (stable), tokio, axum.
- **Modules:**
  - `device_api`
  - `config`
  - `cache`
  - `sources/{icloud, librus, weather}`
  - `dashboards/{family, homelab}`
  - `render`
- **Fetching is separate from rendering.** Fetchers write to the cache on their own
  schedules. A display request only reads the cache and renders, so device requests
  never wait on, or fail because of, an outside service.

### Device API

This follows the firmware's BYOS protocol. Details such as exact header names come from
docs.trmnl.com/go/diy/byos, and the builder must check them against that page.

- **`GET /api/setup`:** the device sends its MAC address in the `ID` header. If the MAC
  is in the config, the server replies with that device's `api_key`, a `friendly_id`
  and an initial `image_url`. Unknown MACs get 404. Devices never register themselves.
- **`GET /api/display`:** the device authenticates with the `Access-Token` header, which
  must match a configured `api_key`. The server:
  1. renders the device's dashboard for "now"
  2. stores the PNG in memory under its content hash
  3. replies with `{ image_url: "<base>/images/<hash>.png", filename: "<hash>",
     refresh_rate: <seconds>, update_firmware: false, reset_firmware: false }`

  The `filename` changes only when the pixels change, so the device skips redraws when
  nothing has changed. Battery, RSSI, firmware version and the other telemetry headers
  are logged at debug level.
- **`POST /api/log`:** the request body is logged and the server replies 204.
- **`GET /images/{hash}.png`:** serves the stored PNG with `Content-Type: image/png`.
  The server keeps the last N images for each device.
- **`GET /preview/{dashboard}.png`:** for development and checking.
  - It accepts optional `at=<RFC3339>` (pretend it's that time) and `depth=1|2`.
  - It has no authentication; it's meant for the LAN only.
  - In dev mode, templates reload from disk automatically.

### Refresh schedule

The server chooses `refresh_rate` for each response:

- Between `wake` (06:00) and `sleep` (22:00): 15 minutes. If `switch_at` comes sooner,
  the refresh is set to land shortly after it, so the switch shows up on time.
- Outside those hours: the number of seconds until the next `wake`, so the device sleeps
  through the night.
- *(builder's-call decision)* There is no separate night screen. `sleep` only affects how
  long the device waits. The last evening image (next school day featured) stays up
  overnight, and the 06:00 refresh brings the morning view.
- All times are in the configured time zone (`Europe/Warsaw`).

### Config

One TOML file at `/data/config.toml`. Secrets are files under `/run/secrets/` (compose
secrets), which the config refers to by file name. See Deployment.

```toml
timezone = "Europe/Warsaw"
base_url = "http://nas.local:8080"      # what devices use to fetch images

[schedule]
wake = "06:00"
sleep = "22:00"
refresh_minutes = 15

[[device]]
name = "kitchen"
mac = "AA:BB:CC:DD:EE:01"
api_key_secret = "trmnl_kitchen_key"
dashboard = "family"
bit_depth = 2            # 2 (default) or 1

[[device]]
name = "office"
mac = "AA:BB:CC:DD:EE:02"
api_key_secret = "trmnl_office_key"
dashboard = "homelab"

[family]
switch_at = "16:00"      # after this, the featured school day is the next one
calendar_days = 7

[family.calendar]
ics_url_secret = "icloud_ics_url"  # public shared-calendar link (webcal:// → https://)

[[family.kid]]
label = "Kid 1"                  # shown as the column heading
librus_login_secret = "librus_kid1_login"
librus_password_secret = "librus_kid1_password"

[[family.kid]]
label = "Kid 2"
librus_login_secret = "librus_kid2_login"
librus_password_secret = "librus_kid2_password"

[family.weather]
latitude = 52.23
longitude = 21.01
```

## Family dashboard

### Layout (800×480, layout "A")

```
┌─────────────────────────────────────────────────────────────────────────┐
│░Środa, 25 listopada░░░░░░░░░░░░░░░░░░░░░░░░☁ 4°░░Pochmurno░░░░░░░░░░░░░░░░│
│░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░maks. 6° min. 1° · deszcz od 15│
├────────────────────────┬────────────────────────┬────────────────────────┤
│ TEN TYDZIEŃ            │ KID 1                  │ KID 2                  │
│ Dziś                   │░Dziś · 8:00–14:25░░░░░░│░Dziś · 8:00–12:35░░░░░░│
│ 16:30 Swimming (Kid 1) │ 8:00  Matematyka       │ 8:00  Ed. wczesnoszk.  │
│ 18:00 Parent–teacher…  │ 8:55  Język polski     │ 8:55  Ed. wczesnoszk.  │
│ czw. 26                │ 9:50  H̶i̶s̶t̶o̶r̶i̶a̶          │ 9:50  Język angielski  │
│ cały dzień Babcia…     │ 10:55 WF               │ 10:55 Religia          │
│ 17:15 Piano (Kid 2)    │ 11:50 Biologia █zast.█ │ 11:50 WF               │
│ pt. 27 …               │ 12:45 Język angielski  │                        │
│ …                      │ czw. · 8:55–13:30      │ czw. · 8:00–11:40      │
│                        │ Fizyka, Chemia, …      │ Ed. wczesnoszk. ×2, …  │
└────────────────────────┴────────────────────────┴────────────────────────┘
```

`░` = light gray (level 170), `█` = black.

**Mockup:** [`2026-11-25-family-dashboard-mockup.html`](2026-11-25-family-dashboard-mockup.html)
is the agreed visual reference. It's self-contained and opens straight from disk:
- the 2-bit and 1-bit reference renders from the Takumi test, embedded as images
- live HTML/CSS mockups of the states before and after `switch_at`, using the same CSS
  as the test template, which makes a good starting point for `templates/family.html`

Inter loads from Google Fonts when online, and a fallback font is used otherwise.

**Tone rule (2-bit palette: 0, 85, 170, 255):**
- **Light gray (170)** is for structural areas: the header band and each kid's day bar,
  with black bold text on top.
- **Black** is for text, rules and alerts. The inverted "zast."/flag tag stays black
  with white text, so it's the one thing that stands out. Cancelled lessons stay struck
  through.
- **Dark gray (85)** is available but unused for now.
- At 1-bit, light gray turns into white (cutoff 160), so the layout degrades cleanly:
  the bands disappear and the bold text keeps the hierarchy.

- **Header:**
  - the date
  - a weather icon, the current temperature and conditions
  - a second line: today's high and low, plus a short precipitation note
- **Calendar column ("TEN TYDZIEŃ"):**
  - covers today plus the following `calendar_days − 1` days, grouped by day
  - today's events drop off once they've ended
  - all-day events are shown as "cały dzień". That's wider than a time, so it must not
    push its title out of line with the other titles: either widen the time column or
    give all-day events their own style without a time (builder's call, check in the
    preview)
  - long titles are cut with an ellipsis
  - roughly 4–5 entries a week are expected, and the column has room for about 12
  - if events don't fit, the overflow becomes a final "+N więcej" line
- **One column per kid:**
  - **Featured day:** a light-gray header bar reading "Dziś · 8:00–14:25" (or "Jutro · …" / "pon. · …"), then each
    lesson with its start time and subject.
    - A cancelled lesson is struck through.
    - A substitution (or another Librus flag) gets a small inverted tag such as "zast.".
  - **Summary line**, below the featured day:
    - before `switch_at`: the next school day as "czw. · 8:55–13:30", plus a comma list
      of subjects (shortened, with ×2 for repeats)
    - after `switch_at`: **today** as "Dziś · 8:00–14:25", times only, so it's clear
      when the kids come home

### Time rules

| Time | Featured day | Summary line | Weather line |
|---|---|---|---|
| Before `switch_at` | today, or the next school day if today has no lessons | next school day after the featured one | rest of today |
| After `switch_at` | next school day | today's start and end times, or nothing if today had no lessons | adds tomorrow's high, low and conditions |

- A **school day** is a date that has at least one lesson, cancelled or not, in that
  kid's Librus timetable. Weekends, holidays and breaks have no lessons, so they're
  skipped automatically. Friday evening therefore features Monday.
- These rules apply to each kid separately, since their timetables differ.
- **No school day within the fetched range** (summer holidays, long breaks): the kid's
  column just shows "Brak lekcji". *(builder's-call decision: no "school starts on…" date,
  because it would need another Librus page or more requests.)*

### Data sources

**iCloud calendar (ICS):**
- The family calendar is shared as a public read-only link (iCloud → Share Calendar →
  Public Calendar). The `webcal://` URL is fetched as `https://`.
- It's fetched every 15 minutes, with `ETag`/`If-Modified-Since` when the server
  supports them.
- It's parsed with an ICS crate. **Recurring events (`RRULE`, `EXDATE`,
  `RECURRENCE-ID` overrides) must be expanded** for the visible window. Time zones come
  from `TZID`/`VTIMEZONE` and are converted to the configured zone.
- Not chosen: CalDAV with an app-specific password. We'll switch to it only if the
  public link turns out to be unacceptable.

**Librus Synergia (one account per kid):**
- This is a Rust port of the login and timetable parts of `Mati365/librus-api`
  (MIT). We port the logic, not the dependency.
- **Login.** This is the same flow the browser uses (portal
  `portal.librus.pl/rodzina/synergia/loguj` → … → `synergia.librus.pl/rodzic/index`),
  minus the portal page itself, which is only the HTML landing page that sends the
  browser into the chain. Following `Mati365/librus-api` (`lib/api.js`, `authorize`):
  1. `GET synergia.librus.pl/loguj/portalRodzina` with `Referer: https://portal.librus.pl/`
     and redirects not followed. Its `Location` is `api.librus.pl/OAuth/Authorization…`
     and carries the OAuth state.
  2. `GET` that URL, then `GET` its `Location` (the login form, also on
     `api.librus.pl/OAuth/Authorization…`).
  3. `POST` the form fields `action=login`, `login`, `pass` (urlencoded) to the login
     URL with:
     - the `x-baner` header (a random number and the current timestamp, each
       character shifted by +20 code points, joined with `_`)
     - `X-Requested-With: XMLHttpRequest`
     - `Accept: application/json, text/javascript, */*; q=0.01`
     - `Referer` set to the login URL and `Origin: https://api.librus.pl`
  4. Follow the continuation (the `Location` header or the JSON `goTo` field) by hand,
     for at most 10 hops. Each hop must stay on `api.librus.pl/OAuth/Authorization…`
     or `synergia.librus.pl/loguj/portalRodzina…`.
  5. **End condition (differs from upstream).** A redirect to
     `synergia.librus.pl/rodzic/…` or `synergia.librus.pl/uczen/…` means the login
     succeeded: stop, don't follow it, keep the cookies. The browser lands on
     `/rodzic/index` for a parent login and, presumably, `/uczen/index` for a student
     login. Upstream's URL check only allows `/loguj/portalRodzina` in the chain, so it
     would reject a plain 302 to `/rodzic/index`. That suggests the last hop is
     currently a 200 page, but **the owner's captured responses decide this**, and the
     parser must handle both cases.
  6. Any other host or path ends the login with an error. A cookie jar is kept for each
     kid, and the session cookies are scoped to `synergia.librus.pl`.
- **Parent and student logins both work.** Both kinds of Synergia login go through the
  same chain, and `/przegladaj_plan_lekcji` sits at the site root for both, so nothing
  else differs.
- **Timetable:** `POST /przegladaj_plan_lekcji` as a multipart form with
  `tydzien=YYYY-MM-DD_YYYY-MM-DD`. The response is parsed as HTML:
  - lesson rows are `table.plan-lekcji tr.line1`; `line0` rows (breaks) are skipped
  - the row header holds the lesson times
  - lesson cells hold `.text` with the subject in `<b>` and "teacher s. room" after it
  - a lesson wrapped in `<s>` is cancelled
  - `.plan-lekcji-info` holds flags
- Each fetch covers **the current week and the next week**, so "next school day" works
  on Friday evenings and before breaks.
- **Schedule:** every 2 hours from 05:30 to 21:30, plus once at startup. That's about
  9 logins per account per day, which picks up same-day substitutions without looking
  like scraping. A failed login backs off exponentially, up to the next scheduled slot.
- **Fragility:** the upstream login flow broke at least three times in 2026. The parser
  and login get fixture-based tests (saved HTML), so a breakage shows up as a clear log
  error, while the screen keeps the cached timetable.
- **One Synergia login for each kid (decided).** This can be the kid's own student login
  or the parent login tied to that kid; the login flow is the same for both. Each kid
  gets its own credentials, cookie jar and session. There's no "choose child" step and
  no single Portal Rodzina account covering several children.
- **Test fixtures come from the owner.** The binary has a `capture` subcommand (for
  example `tablica capture librus --out fixtures/librus/`). For each configured
  kid it logs in once, saves the raw login-chain responses (status, headers, body) and
  the timetable HTML for the current and next week, and then exits. Names of the kids,
  teachers and the school are blanked out on a best-effort basis, and the owner checks
  the result before committing it. The owner runs it; the builder develops against the
  saved files and doesn't log in to Librus during development. **So `capture` is built
  before the parser**, so the owner can produce the fixtures early.

**Weather (Open-Meteo, no API key):**
- Fetched every 30 minutes for the configured latitude and longitude.
- It requests:
  - current temperature and weather code
  - daily high, low and weather code for today and tomorrow
  - hourly precipitation probability, used for the "deszcz 30% od 15:00" note (the
    first hour today with ≥ 30%)
- Weather codes map to a small set of icons (sun, partly cloudy, cloud, rain, snow,
  storm, fog), drawn as inline SVG for crisp 1-bit and 2-bit output.

### Staleness and failure

- The cache stores the last good payload and `fetched_at` for each source, and writes
  them to `/data/*.json` so they survive restarts.
- A source counts as **stale** once it's older than:
  - calendar: 2 hours
  - timetable: 26 hours
  - weather: 3 hours
- When stale, that section shows a small footnote such as "stan na pon. 07:30". The data
  is still drawn.
- If a source has **never** succeeded, that section shows a neutral placeholder such as
  "Kalendarz niedostępny", never an error message.
- When the renderer itself fails (which shouldn't happen), `/api/display` re-serves the
  device's last successful image.

## Rendering pipeline

1. **Build the view model.** `dashboards::family::build_view(now, &cache, &config) -> FamilyView`.
   This is pure logic, it's fully unit-tested, and it takes the clock as a parameter. It
   decides the featured day and summary, groups events, shortens subjects, picks icons
   and adds stale notes. It produces final strings, but not pixel positions; layout is
   left to CSS.
2. **Render the template.** MiniJinja renders `templates/family.html`, which holds the
   HTML with an inline `<style>`.
   - Release builds embed the templates in the binary (`minijinja-embed`).
   - Dev builds load them from disk with autoreload.
3. **Lay out and draw.** Takumi does this:
   - `parse_html`, then the `<style>` CSS becomes the stylesheet
   - `render` at a viewport of 800×480
   - the **Inter** variable font (OFL) is embedded with `include_bytes!` and registered
     once at startup
4. **Quantize.** The result is composited on white and converted to luminance, then:
   - **2-bit:** rounded to 4 levels (0, 85, 170, 255)
   - **1-bit:** thresholded at 160

   There's no dithering, since this is text and line art.
5. **Encode** to PNG and check that it's ≤ 90,000 bytes. Going over is logged as an
   error. It isn't expected for this content.

- **Pinning:** `takumi` is pinned to an exact version (`=2.22.1`) and only upgraded on
  purpose, after checking the preview.
- **Takumi calls that worked in the test**, with `takumi = { version = "=2.22.1",
  features = ["from-html"] }`. The test code itself was throwaway, so these are the only
  record of it:

  ```rust
  use takumi::prelude::*;
  let mut fonts = Fonts::default();
  fonts.register(FontResource::new(INTER_TTF))?;          // variable TTF; font-weight maps to wght
  let parsed = takumi::parse_html(&html, FromHtmlOptions::default())?;
  let sheet = StyleSheet::parse_loosy(&parsed.css.join("\n"));
  let opts = RenderOptions::builder()
      .viewport(Viewport::new((800, 480)))
      .node(parsed.node)
      .fonts(&fonts)
      .stylesheet(sheet.into())
      .build();
  let bitmap = takumi::render(opts)?;                      // then write_image(.., OutputFormat::Png) or raw RGBA
  ```
- **Text colors:** templates use only black, white and the two grays the panel can
  show, so text stays crisp.
- The test showed a render takes about 7 ms, so rendering on every request is fine.

## Deployment

This follows the conventions of the owner's `bwilczynski/twydo` project, which runs on
the same NAS:

- **Toolchain pinning:**
  - `mise.toml` pins Rust (with clippy and rustfmt) and holds the `build`, `test` and
    `lint` tasks
  - `rust-toolchain.toml` mirrors the same version for CI
  - edition 2024
- **Dockerfile** at `contrib/docker/Dockerfile`, built from the repo root:
  - a `rust:<pinned>-bookworm` build stage running `cargo build --release --locked`
  - a `debian:bookworm-slim` runtime with only `ca-certificates` added
  - a non-root user with uid 1000, a `/data` volume, and one exposed port
  - the binary is the entrypoint directly, with no `tini` and no entrypoint script.
    twydo needs them to supervise two processes; this server has only one and starts
    no child processes, so there's nothing to reap. As PID 1, though, it gets no
    default signal handling, so **the binary must handle SIGTERM and SIGINT itself**
    (tokio `signal`, axum graceful shutdown). Otherwise `docker stop` waits out its
    10-second timeout and kills it.
- **`contrib/docker/docker-compose.yaml`:**
  - the `ghcr.io/bwilczynski/tablica:latest` image
  - `restart: unless-stopped`, with `./data:/data` holding the config and cache
  - the port published on the LAN, since the devices must reach it (unlike twydo,
    which binds to localhost)
- **Secrets are compose `secrets:` files**, like twydo's `caldav_password`. The config
  refers to each secret by the name of a file under `/run/secrets/` (for example
  `librus_kid1_password`). Every `*_secret` key also accepts a `*_env` form, which
  names an environment variable instead, for local development.
- **CI (GitHub Actions):**
  - `ci.yml`: test, clippy and fmt
  - `image.yml`: a smoke build of the image on pull requests (build, run, then check
    `/healthz`), and on `v*` tags a **multi-arch build for `linux/amd64` and
    `linux/arm64`** pushed to GHCR under one manifest
  - the multi-arch build works whatever the NAS's CPU is
- **Health and logs:**
  - a `/healthz` endpoint reports whether the server is alive, plus how fresh each
    source is
  - logs go to stdout with `tracing`
- **Releases** are created with release-plz and commits follow Conventional Commits, as
  in twydo. Release automation can come later and doesn't block the first deploy.

## Testing

- **Unit tests for the view-model rules**, with a fixed clock. They cover:
  - before and after the switch
  - Friday evening jumping to Monday
  - a holiday week
  - an empty day
  - the "+N więcej" overflow
  - stale notes
- **ICS parsing tests:** recurring events, exceptions, all-day events, and events in
  other time zones.
- **Librus tests:**
  - timetable parser tests against saved HTML fixtures, with personal data scrubbed
  - the login flow tested against a mock HTTP server that replays the redirect chain
- **Render tests:**
  - the full pipeline produces a valid 800×480 PNG within the size limit
  - an optional golden-image comparison, with a tolerance, for layout regressions
- **Device API tests:** setup with a known and an unknown MAC, display with a bad
  token, and the `refresh_rate` calculation around wake, sleep and switch times.

## Homelab dashboard (placeholder)

- Wired up as a second `dashboards::homelab` that returns a simple "Homelab: wkrótce"
  view through the same pipeline.
- Its data sources and layout get their own design later.

## Open questions (to confirm on real hardware)

- **How the 2-bit PNG must be encoded:** 2-bit grayscale (color type 0) or indexed
  (color type 3), and whether the device has to announce or be set to 2-bit mode.
  Start with 2-bit grayscale. If the device shows it wrongly, try indexed, then fall
  back to `bit_depth = 1`.
- **Refresh speed and ghosting in gray mode** on the DIY kit.
- **Which OG variant the DIY kit counts as** (`og` model header) and its firmware
  version.

## Left to the builder

- Crate choices beyond the ones named here (ICS parsing and RRULE expansion, HTTP
  client and cookie jar, HTML parser, PNG encoder).
- Module layout below the top-level modules.
- Exact template CSS (spacing, font sizes), tuned with the preview endpoint so it
  matches the agreed mockup.
- How many images to keep, and log levels.
