# PUMP release gates

All changes accumulate in `fix/playback-stability`, draft PR #2. No intermediate merges. A gate is PASS only when its acceptance criteria have evidence against the final candidate commit; implemented code is not evidence of successful runtime behavior.

| Gate | State | Acceptance criteria |
|---|---|---|
| 0 Audit | IN PROGRESS | Inventory entry points, provider and trust boundaries, known defects, regression coverage, reproducible baseline. |
| 1 Streaming | IN PROGRESS | Default search prioritizes playable complete recordings, providers independently fail, stable IDs, valid Range/seek, no preview mislabeling, public-source authorization and license review, live full-stream check. |
| 2 Rooms | IN PROGRESS | Clock estimation, scheduled start/prebuffer, drift correction, pause/seek/switch/reconnect, simulated latency/jitter, measured two-device playback alignment. |
| 3 Lyrics | IN PROGRESS | Duration-aware matching, overlapping vocals, forward/backward seek, pause, track switch and offline, saved per-track timing adjustment; mobile checks. |
| 4 DSP | IN PROGRESS | Crossfade, Automix, analysis integrated into actual playback and tested with real audio; CPU and memory budgets. |
| 5 Quality | IN PROGRESS | Browser/mobile compatibility, bounded resource usage, security review and performance measurements. |
| 6 Release | OPEN | All gates PASS, full CI on current head, documented limits and reproducible setup; one ready PR. |

## Evidence so far

- Default search now runs SoundCloud, YouTube and iTunes concurrently in core; duplicate API-layer SoundCloud search removed, result IDs deduplicated. Stub regression assertions added. Audius provider now wired into search, track lookup, browser and guarded Range proxy; fixture tests added. Live public API/content-node playback still requires verification.
- Room updates use bounded host timestamps, rolling RTT estimate, monotonic client clock, playback-rate correction and bounded reconnect. Regression for reconnect added. Shared future playback anchors and host/guest scheduled start implemented; browser timeline test added. True readiness handshake, network jitter test, and physical output-latency measurement remain open.
- Lyrics requests include duration when known; word intervals are independent, paused seeks repaint, per-track offset is locally persisted. Regression assertions added. Preview lyrics are now plain-only instead of showing wrong timestamps; overlapping line and word intervals use indexes. Added tests for vocal overlap, gaps, reverse seeks and offsets. Mobile verification remains open.
- Workflow 46048448956: Docker and all-features Clippy passed, rustfmt failed; formatting corrected in 735ebe94d8262e1482b3eb7db4152855271a617b. Workflow 46049022457: native passed; test and Docker cancelled by subsequent commits. Neither establishes final candidate readiness.
- Audius: official REST API https://docs.audius.co/api/ with public-streamable metadata filter, explicit gating checks, bounded JSON, exact redirect allowlist, no app download. App key/content-node allowlist may be required; live verification remains open.
- SoundCloud: fixed inherited 9 s total request timeout for full streaming and terminal idle/error behavior; uses shared bounded proxy with Range support.
- DSP: local analysis API uses existing Rust Symphonia + MusicAnalyzer, one blocking worker, bounded PCM/file size and metadata-keyed cache. Browser dual-deck Crossfade shares WebAudio/Rust DSP chain; Automix uses cached BPM, beatgrid, LUFS, bounded tempo adjustment and fallback. Added real local audio and crossfade browser test. No physical audio-output verification yet.
- JavaScript syntax parsed successfully via V8 after audio integration. The previous CI runs 46049909276 and 46050763669 failed rustfmt; the reported formatting corrections are committed. Final candidate CI must run without cancellation.

## Verified baseline: 32a095b

[CI run 55](https://github.com/arksne/PUMP/actions/runs/46052511739) completed successfully: rustfmt, workspace tests, analysis tests, Clippy (all features), WASM checks, Docker and browser tests. Browser suite: 12 passed, 3 live SoundCloud tests skipped because the service was unavailable from the runner. This verifies the local provider fixtures, reconnect, lyrics intervals, analysis and crossfade on that commit, not real-world external streams or speaker-output latency.

The next candidate adds a persisted listener-relative manual offset, room RTT/timeline diagnostics and a deterministic delayed/dropped WebSocket message test. Re-run CI on that candidate; the displayed position error is not a physical latency measurement. Set PUMP_REQUIRE_SOUNDCLOUD_LIVE=1 when running tests/browser.py to treat unavailable live SoundCloud as a failure instead of a skip.

## Blocking verification

Physical Telegram/mobile devices, external account-dependent providers and real-world room output latency are not verified by the GitHub connector. Record actual measured evidence when available. Do not disable checks or infer passing tests from compilation alone.
