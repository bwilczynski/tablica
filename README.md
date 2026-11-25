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
