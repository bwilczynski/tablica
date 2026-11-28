# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.7](https://github.com/bwilczynski/tablica/compare/v0.1.6...v0.1.7) (2026-11-28)


### Added

* add metrics label sanitising ([88bf063](https://github.com/bwilczynski/tablica/commit/88bf0639c5b20e02e9755a46011b8531e8cf3a0f))
* add metrics_listen config key ([9fa7928](https://github.com/bwilczynski/tablica/commit/9fa7928041440bdae3d0017f2d2d2efc16c180a1))
* add the Grafana dashboard for tablica ([64e9747](https://github.com/bwilczynski/tablica/commit/64e9747a1fab6ec5e6dba5699fdd5820630bd0c6))
* add the metrics recorder ([ba57352](https://github.com/bwilczynski/tablica/commit/ba573525cdba23b4c7bc07d5beb2f69a1c40f501))
* collect device, source and process metrics at scrape time ([5ecf792](https://github.com/bwilczynski/tablica/commit/5ecf79299c81198ec7420cf4ba46fb441f4cecba))
* expose source status for metrics ([e96f5d5](https://github.com/bwilczynski/tablica/commit/e96f5d57616a223499020fdb9d7e9ae4b544f96b))
* keep per-device metrics state ([15900a0](https://github.com/bwilczynski/tablica/commit/15900a0736b4abb98b09a0d6fcd12b2661dcf007))
* read the metrics seed from history ([7936fb8](https://github.com/bwilczynski/tablica/commit/7936fb83a3b9672e43dfa44840dd8b842cf178d8))
* record HTTP, render and device metrics ([67fd9bb](https://github.com/bwilczynski/tablica/commit/67fd9bb937064d440ff392bb0002c2d208e0b4aa))
* record poller outcomes and durations ([8a93c77](https://github.com/bwilczynski/tablica/commit/8a93c7764a3b76e01629364ecbf7bfe0fd56390a))
* serve /metrics on metrics_listen ([2fc1212](https://github.com/bwilczynski/tablica/commit/2fc12128c384788e5b5b1b7b66755431fae09bd4))
* start problem request and render failure series at zero ([b805ac9](https://github.com/bwilczynski/tablica/commit/b805ac918fa2e16745c8cd176840e4c119c8e443))

## [0.1.6](https://github.com/bwilczynski/tablica/compare/v0.1.5...v0.1.6) (2026-11-27)


### Added

* derive battery, Wi-Fi and reset status from history ([e1afdf7](https://github.com/bwilczynski/tablica/commit/e1afdf798a9f8b181dd9adf25a97ab2e71a6181c))
* keep device telemetry and logs in a SQLite history ([4345ce8](https://github.com/bwilczynski/tablica/commit/4345ce8033614e2a7bb265e1fe493c9e4db9c51c))
* let dashboards contribute diagnostics sections ([816fd6c](https://github.com/bwilczynski/tablica/commit/816fd6c78795ef3ff591508c9087d8ab1db93108))
* record device telemetry and log entries in the history ([3420871](https://github.com/bwilczynski/tablica/commit/3420871475220c273bb9a131ac7eb730a730dc3f))
* render the device diagnostics screen ([fa9be56](https://github.com/bwilczynski/tablica/commit/fa9be56630dd7b0bf1e1ed19836d696c13cfb750))
* serve the diagnostics screen at /preview/diag.png ([3395561](https://github.com/bwilczynski/tablica/commit/33955614a294152c6a4f3412e8925898d33e5c41))


### Fixed

* reject two devices with the same name or MAC address ([61b94d6](https://github.com/bwilczynski/tablica/commit/61b94d631189cf45fa810b546e3367864d194778))

## [0.1.5](https://github.com/bwilczynski/tablica/compare/v0.1.4...v0.1.5) (2026-11-27)


### Added

* log every device API request in one access line ([a5886f7](https://github.com/bwilczynski/tablica/commit/a5886f7ea537680a0d4e1766f6cc861201998568))


### Fixed

* log the device MAC when a display token is unknown ([c23a8a3](https://github.com/bwilczynski/tablica/commit/c23a8a3139548123fe51b1948b53b3ce8280c233))

## [0.1.4](https://github.com/bwilczynski/tablica/compare/v0.1.3...v0.1.4) (2026-11-27)


### Fixed

* keep response content out of source error messages ([25da944](https://github.com/bwilczynski/tablica/commit/25da94448bff0bd9281bda035cdc22cf67b657e4))

## [0.1.3](https://github.com/bwilczynski/tablica/compare/v0.1.2...v0.1.3) - 2026-11-26

### Added

- *(family)* feature today's events above the week list in the calendar

### Fixed

- *(family)* keep tomorrow in range and fit events before the empty week

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
