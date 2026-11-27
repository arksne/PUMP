# PUMP release gates

All changes accumulate in `fix/playback-stability`, draft PR #2. No intermediate merges. A gate is PASS only when its acceptance criteria have evidence against the final candidate commit; implemented code is not evidence of successful runtime behavior.

| Gate | State | Acceptance criteria |
|---|---|---|
| 0 Audit | IN PROGRESS | Inventory entry points, provider and trust boundaries, known defects, regression coverage, reproducible baseline. |
| 1 Streaming | IN PROGRESS | Default search prioritizes playable complete recordings, providers independently fail, stable IDs, valid Range/seek, no preview mislabeling, public-source authorization and license review, live full-stream check. |
| 2 Rooms | IN PROGRESS | Clock estimation, scheduled start/prebuffer, drift correction, pause/seek/switch/reconnect, simulated latency/jitter, measured two-device playback alignment. |
| 3 Lyrics | IN PROGRESS | Duration-aware matching, overlapping vocals, forward/backward seek, pause, track switch and offline, saved per-track timing adjustment; mobile checks. |
| 4 DSP | OPEN | Crossfade, Automix, analysis integrated into actual playback and tested with real audio; CPU and memory budgets. |
| 5 Quality | OPEN | Browser/mobile compatibility, bounded resource usage, security review and performance measurements. |
| 6 Release | OPEN | All gates PASS, full CI on current head, documented limits and reproducible setup; one ready PR. |

## Evidence so far

- Default search now runs SoundCloud, YouTube and iTunes concurrently in core; duplicate API-layer SoundCloud search removed, result IDs deduplicated. Stub regression assertions added. New full-audio provider not integrated yet.
- Room updates use bounded host timestamps, rolling RTT estimate, monotonic client clock, playback-rate correction and bounded reconnect. Regression for reconnect added. Scheduled start and output-latency compensation remain open.
- Lyrics requests include duration when known; word intervals are independent, paused seeks repaint, per-track offset is locally persisted. Regression assertions added. Preview alignment and mobile verification remain open.
- Workflow 46048448956: Docker and all-features Clippy passed, rustfmt failed; formatting corrected in 735ebe94d8262e1482b3eb7db4152855271a617b. Workflow 46049022457: native passed; test and Docker cancelled by subsequent commits. Neither establishes final candidate readiness.
- Audius candidate: official REST search/stream API https://docs.audius.co/api/ supports public tracks. Before integration, verify explicit public-access metadata and API terms, response schema, CORS/Range and redirects with a strict host policy. Do not assume every catalog entry is publicly playable or downloadable.

## Blocking verification

Physical Telegram/mobile devices, external account-dependent providers and real-world room output latency are not verified by the GitHub connector. Record actual measured evidence when available. Do not disable checks or infer passing tests from compilation alone.
