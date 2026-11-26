# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2](https://github.com/bwilczynski/tablica/compare/v0.1.1...v0.1.2) - 2026-11-26

### Fixed

- fetch at startup when there is no cached data

## [0.1.1](https://github.com/bwilczynski/tablica/compare/v0.1.0...v0.1.1) - 2026-11-26

### Added

- poll Librus on timetable-aware jittered schedule
- *(family)* add timetable-aware Librus poll policy with kid staggering
- *(family)* derive Librus polling windows from the timetable
- *(polling)* add generic poll policy, runner and jitter
- show event location and shade day bands in the calendar

## [0.1.0](https://github.com/bwilczynski/tablica/releases/tag/v0.1.0) - 2026-11-26

### Added

- serve TRMNL device API
- render dashboards to 2-bit and 1-bit PNG
- build family dashboard view model
- schedule source fetchers
- parse Librus timetable and fetch two weeks
- add Librus login flow and capture command
- add ICS calendar source with recurrence expansion
- add Open-Meteo weather source
- add JSON-backed source cache
- add Polish on-screen text module
- scaffold tablica crate and config loading
