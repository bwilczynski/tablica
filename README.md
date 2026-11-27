# tablica

A small, self-hosted backend for TRMNL e-ink displays, built for a single household and
configured once.

<p align="center">
  <img src="docs/images/family-morning.png" width="800" alt="Family dashboard in the morning: today's events and the week on the left, each kid's lessons for today on the right">
</p>

<p align="center"><em>The family dashboard as the device shows it (sample data).</em></p>

## Features

### Family dashboard

- **Calendar:** today's events at the top, then the rest of the week grouped by day,
  from a shared iCloud calendar (ICS). Recurring events, exceptions and all-day events
  are handled, locations are shown under the title, and anything that does not fit
  collapses into a "+N więcej" line.
- **School timetables:** one column per kid from Librus Synergia, with each lesson's
  start time, cancelled lessons struck through and substitutions flagged, plus a
  one-line summary of the next school day.
- **Weather:** current temperature and conditions from Open-Meteo, today's high and low,
  and when rain or snow is likely.
- **Follows the day:** in the morning it features today's lessons; after a configurable
  time (`switch_at`) it moves on to the next school day and tomorrow's forecast, while
  still showing when today's lessons end.

<p align="center">
  <img src="docs/images/family-evening.png" width="600" alt="Family dashboard in the evening: tomorrow's lessons are featured and today's are summarised">
</p>

<p align="center"><em>The same dashboard after <code>switch_at</code>: tomorrow comes first.</em></p>

### Under the hood

- **Gentle on Librus:** each kid's timetable is polled more often in the hours before
  school and the evening before a school day, sparsely during lessons and hardly at all
  on days off, with random jitter and the kids staggered apart.
- **Crisp on e-ink:** 2-bit grayscale images (4 shades), with a 1-bit option for each
  device.
- **Polish on screen:** everything the dashboards show is in Polish.
- **No browser needed:** HTML/CSS templates are laid out and rendered in Rust. Pass
  `--templates <dir>` to `tablica serve` to use your own templates; edits are picked up
  without a restart.
- **Survives outages:** when a source is down, the screen keeps the last good data with
  a "stan na …" (as of) note.
- **Battery-friendly:** refreshes every 15 minutes between wake and sleep times, wakes
  just after `switch_at` so the switch shows up on time, and sleeps through the night.
- **Configured, not managed:** one config file, secrets kept in files, and no admin UI
  and no database server.
- **One container:** a single Rust binary, published as a multi-arch (amd64/arm64)
  Docker image.

### Diagnostics

Each device has a diagnostics screen: battery level and drain estimate, Wi-Fi signal,
the last reset that was not a normal wake, the last device log, and a section from its
dashboard (for the family dashboard, how fresh each source is).

- **What is recorded:** the telemetry headers each device sends with `/api/display`
  (battery voltage, RSSI, wake time, reset reason, firmware and so on) and the log bodies
  it posts to `/api/log`. Recording never changes or delays a device reply.
- **Storage:** a SQLite file at `data_dir/history.db`. Readings and logs are kept for 90
  days, and at most 2,000 log rows and 20,000 readings per device. The file does not
  shrink: it stays at its high-water mark, and freed space is reused. If the file is
  corrupt it is set aside as `history.db.corrupt-*` (the newest 3 sets are kept) and a
  fresh one is started. Any other problem (or a fresh file that fails too) disables
  history: the screen says history is unavailable and the server carries on.
- **Browser access:** `/preview/diag.png?device=<name>` renders the screen as a PNG, with
  optional `depth=1` or `2` and an RFC 3339 `at=` to see the history as of an earlier
  moment. The dashboard's sections are always current, whatever `at` says.

### Planned

- **Homelab dashboard:** a device can already be pointed at it, but it shows a
  placeholder for now.

## Running

Use the compose example in `contrib/docker/docker-compose.yaml`:

1. Copy `config.sample.toml` to `./data/config.toml` and edit it. `./data` is mounted at
   `/data` in the container and also holds the cache. It must be writable by uid 1000
   (`sudo chown -R 1000:1000 data`).
2. Put each secret in its own file under `./secrets/`, named as in the config
   (`trmnl_kitchen_key`, `librus_kid1_password` and so on). Compose mounts them at
   `/run/secrets/`.
3. Run `docker compose -f contrib/docker/docker-compose.yaml up -d`. The server listens
   on port 8080, and `/healthz` reports its state.

To capture Librus fixtures from a real account, run
`tablica capture librus --config <config> --secrets-dir <dir> --out <dir>`.

## Contributing

The repository is a Cargo workspace:

- `crates/core` (`tablica-core`): device API, rendering, polling, cache, config and
  server. It knows nothing about any particular dashboard.
- `crates/family` (`tablica-family`): the family dashboard.
- `crates/homelab` (`tablica-homelab`): the homelab placeholder.
- `crates/tablica`: the binary, which lists the dashboards in `entries()` in `main.rs`.

```sh
mise run lint    # clippy and rustfmt, whole workspace
mise run test    # cargo test --workspace
```

### Adding a dashboard

1. Create `crates/<name>`, with a name of 5 to 7 lowercase letters. Copy `crates/homelab`
   for the shape: `Cargo.toml`, a `build.rs` that embeds `templates/`, and `src/lib.rs`.
   The name must be unique and not one of `timezone`, `base_url`, `listen`, `data_dir`,
   `schedule` or `device`.
2. Construct a `DashboardEntry` and implement `ParsedDashboard` and `Dashboard` from
   `tablica-core`, and export an `entry()` function:
   - parsing the `[<name>]` config section happens offline;
   - `prepare` opens caches and clients, without touching the network;
   - `spawn_pollers` runs after the server has bound its port;
   - `health` returns sanitized JSON only (no secrets, no raw error text).
3. Optionally override `Dashboard::diagnostics` to list what the dashboard knows about
   its sources on the diagnostics screen. Use `diagnostics::source_row` to turn a source's
   cache into a row with its age and status.
4. Add the crate as a dependency of `crates/tablica` and add its `entry()` to `entries()`
   in `crates/tablica/src/main.rs`.

Caches for a new dashboard go under `data_dir/<name>/`. No release configuration is
needed: releases cover the whole repository.

`cargo test write_previews` renders the dashboards with sample data into
`target/preview/`; the screenshots in `docs/images/` come from there.

Commits and pull request titles follow [Conventional Commits](https://www.conventionalcommits.org/).
`feat`, `fix` and `perf` changes open a release PR (version bump and `CHANGELOG.md`) via
[release-please](https://github.com/googleapis/release-please); merging it tags the
version, creates a GitHub release and publishes the image to
`ghcr.io/bwilczynski/tablica` as `<version>` and `latest`. Other types (`refactor`, `chore`,
`docs`, ...) do not open one unless marked breaking (`!`). The release workflow needs the
`RELEASE_PLZ_TOKEN` repository secret (historical name): a fine-grained PAT for this repository
with Contents and Pull requests read/write, and possibly Issues read/write for the release labels.

If a release fails after the tag was pushed, the image still publishes: create the missing
release by hand from the existing tag, with that version's `CHANGELOG.md` section as notes
(`gh release create vX.Y.Z --notes-file ...`). If the release PR is left labelled
`autorelease: pending` after its release exists, relabel it `autorelease: tagged` so
release-please does not retry it.

## License

MIT, see [LICENSE](LICENSE). The Librus login flow is ported from
[librus-api](https://github.com/Mati365/librus-api) (MIT), and the Inter font is under
the SIL Open Font License; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
