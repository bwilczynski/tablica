# tablica

A small, self-hosted backend for TRMNL e-ink displays, built for a single household and
configured once.

## Features

- **Family dashboard**
  - The coming week from a shared iCloud calendar.
  - Each kid's school timetable from Librus Synergia, including cancelled lessons and
    substitutions.
  - Current weather and today's forecast from Open-Meteo.
  - Shifts focus during the day: in the morning it shows today's lessons, and in the
    afternoon it moves on to the next school day while still showing when today's
    lessons end.
- **Homelab dashboard** (planned).
- **Crisp on e-ink:** 2-bit grayscale images (4 shades), with a 1-bit option for each
  device.
- **Polish on screen:** everything the dashboards show is in Polish.
- **No browser needed:** HTML/CSS templates are laid out and rendered in Rust.
- **Survives outages:** when a source is down, the screen keeps the last good data with
  an "as of" note.
- **Battery-friendly:** refreshes every 15 minutes during the day and sleeps at night.
- **Configured, not managed:** one config file, secrets kept in files, and no admin UI
  or database.
- **One container:** a single Rust binary, published as a multi-arch (amd64/arm64)
  Docker image.

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

## License

MIT, see [LICENSE](LICENSE). The Librus login flow is ported from
[librus-api](https://github.com/Mati365/librus-api) (MIT), and the Inter font is under
the SIL Open Font License; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
