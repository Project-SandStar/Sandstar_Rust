# Sandstar Rust — Research Docs vs Code, 2026-11-18

**Baseline:** `master` `b48484b`, workspace 2.9.0. The field device runs `2.9.0+3b8e97c`.
**Supersedes:** [PROGRESS_REPORT_2026_04_10.md](PROGRESS_REPORT_2026_04_10.md) and [STATUS_REPORT_2026_04_09.md](STATUS_REPORT_2026_04_09.md).
**Current project snapshot:** [`../Progress/OVERVIEW.md`](../Progress/OVERVIEW.md).

## Method

Every research doc (00-19) and every plan and guide in `../Progress/` was read against the code. The check was done by six read-only audit passes. The key claims of each pass were then re-checked in the code before anything was written. The results live in two places:
- **A "Status (2026-11-18)" block at the top of each document.** It covers what was implemented, what was done differently, what is open, what supersedes the doc, and the known inaccuracies in the original text (by section).
- **This report**, which summarises those blocks.

The original document text was kept as design history. Device addresses, SSH details, site names and developer paths were replaced with placeholders.

## How to read the percentages

The April report scored a doc 100% when its *goal* was met, even if the design in the doc was not used. This report separates the two:
- **Design as written:** how much of the doc's own proposal exists in code.
- **Functional goal:** how much of what the doc wanted to achieve exists, by any design.

Both numbers are estimates from the audit, not measurements.

## Dashboard

| Doc | Title | Design as written | Functional goal | April claim | Main reason for the gap |
|---|---|:-:|:-:|:-:|---|
| 00 | Executive Summary | — | ~70% accurate | 100%+ | v1.0.0 snapshot. Says the VM stays C and libhaystack is used; metrics and "0 security issues" are out of date |
| 01 | Engine Core | ~70% | ~90% | 100%+ | Different data structures (bitflags, `Vec` watch/notify). Open: `is_finite` on outputs, unvalidated `clamp` bounds |
| 02 | Hardware Drivers | ~20% | ~80% | 85% | sysfs + raw libc instead of gpio-cdev/i2cdev/serialport. Open: no async I/O, no I2C read timeout, no GPIO edges |
| 03 | Haystack Type System | 0% | ~30% | 98% | libhaystack not adopted; small in-house subset |
| 04 | REST API / Axum | ~55% | ~70% | 85% | No `watchList`/`hisWrite`/`commit`/`root`/`xeto`. No graceful shutdown. Write routes outside auth |
| 05 | Zinc I/O | 0% | ~60% | 95% | In-house encoder and parser. No Zinc request bodies, although `/api/formats` advertises them |
| 06 | Sedona FFI Strategy | 0% | n/a | 97% | Superseded: the VM was rewritten in Rust |
| 07 | IPC Bridge | 0% | ~50% | 85% | Superseded: single process, bridge snapshot, bincode CLI socket |
| 08 | Memory Safety | — | ~70% holds | 100% | `unsafe` in SVM natives and HAL; no SIGTERM handler; `overflow-checks` off |
| 09 | Dependency Mapping | ~40% | ~90% | 70% | Half the crates not adopted, many added |
| 10 | Build / Cross-compile | ~55% | ~90% | 85% | Zig linker; no C step; CI builds neither `svm` nor `.deb` |
| 11 | Migration Roadmap | ~60% | ~90% | 95% | End goal reached by a different route |
| 12 | Sedona VM Architecture | ~80% | ~80% | 100% | Ported without `unsafe` in the core. Natives get no VM memory |
| 13 | VM Porting Strategy | ~60% | ~50% | 95% | VM cannot run real scode and is not started in production. No fuzzing or benchmarks |
| 14 | VM Scalability | ~25% | ~25% | 90% | `ComponentStore`/`VmConfig` unused; production ids are u16 |
| 15 | SOX/WebSocket → ROX | ~40% | ~60% | 55% | No ROX on :7070; JSON WebSockets on the REST port instead |
| 16 | roxWarp Protocol | ~45% | ~60% | 88% | Typed binary Trio and string table not on the wire. No join/leave; no tie-break |
| 17 | Name Length | ~50% | ~50% | 90% | Intern table built, 31-char limit kept |
| 18 | Driver Framework v2 | ~60% | ~65% | 70% | Modbus not loadable; COV never starts; REST-created drivers not polled |
| 19 | Dynamic Slots | ~55% | ~60% | 78% | No driver auto-population, no ROX propagation |

## Cross-cutting findings

1. **libhaystack was never adopted.** Docs 03, 05, 09 and 11 and parts of 00/01 assume it. Haystack types, Zinc, filter and Trio are in-house, decided on 2026-03-20 (`../Progress/ROADMAP_v2.md` decision log). That covers a small subset of Haystack 4.
2. **There is no C, and the VM is not in production.** The Sedona VM is pure Rust with no `unsafe` in the interpreter core. But natives receive a placeholder buffer instead of VM memory, so real scode cannot run. The packaged service does not start the VM. Control logic runs on the SOX component dataflow (`crates/sandstar-server/src/sox/`).
3. **Security posture is weaker than the April docs state.** The packaged unit binds `0.0.0.0` with no auth configured. Even with auth configured, SOX edits, `/live`, `/file/*`, tags, alerts and cluster routes bypass `check_auth`. Only Ctrl+C and SIGHUP are handled; SIGTERM ends the process without graceful shutdown.
4. **Shipped driver features have production gaps:**
   - MQTT does not reconnect, and its status stays `Ok`.
   - BACnet COV subscriptions never start, because `add_watch` has no production caller.
   - REST-created drivers are never polled.
   - Modbus cannot be loaded.
5. **The roxWarp wire format is not the one documented.** The wire carries `rmp_serde` of an internal enum, with `f64` values, and pushes deltas on the heartbeat tick. Any non-Rust peer, such as the planned Fantom pod, would need a new spec.

## Code issues found during the audit

These are recorded in the internal risk register. They were not fixed as part of this documentation step.

| Issue | Where | Note |
|---|---|---|
| roxWarp feed replicates channel values without an `is_finite` check | `crates/sandstar-server/src/roxwarp/cluster.rs` (`channel_feed_loop`) | A NaN would be sent to peers |
| `/api/formats` advertises `text/zinc` with `receive: true`, but all POST handlers take JSON | `crates/sandstar-server/src/rest/types.rs` | Clients that trust the advertisement get 4xx |
| Name intern table never frees entries and `assert!`s at 65,535 names | `crates/sandstar-server/src/sox/name_intern.rs` | Many distinct renames (reachable through unauthenticated SOX routes) would abort the process |
| `expect("dyn_slots lock poisoned")` on REST paths | `crates/sandstar-server/src/rest/tags.rs` | Same class as other lock-poison panics in release builds |
| `nix` dependency is unused | `crates/sandstar-hal-linux/Cargo.toml` | Housekeeping |
| `Cargo.toml` says 2.9.0, the latest tag is v2.9.1, `CHANGELOG.md` stops at 2.8.10 and contains a device address | repo root | Version/changelog/tag should move together |

## Documents changed in this step

- **Research docs 00-19:** status block added; device addresses and developer paths replaced with placeholders in 00, 10, 11, 15 and 17.
- **Old reports:** `STATUS_REPORT_2026_04_09.md` and `PROGRESS_REPORT_2026_04_10.md` marked superseded.
- **Progress snapshots:** `OVERVIEW.md` revised (header, status matrix, route/auth table, deployment, gates, pending list, timeline). `ROADMAP_v2.md` got a status block, corrected phase rows and section notes.
- **Plans and notes:** `IMPLEMENTATION_PLAN_*`, `PURE_RUST_*` and `NOTES_*` got completion/status headers.
- **Operator guides:** `BACNET_SETUP.md`, `MQTT_SETUP.md`, `DRIVER_REST_API.md` and `HARDCODED_LIMITS.md` got verified-correction blocks and small link fixes.
- **`DEPLOYMENT_CHECKLIST.md`:** rewritten as the current deploy and rollback runbook.

## Not verified

- ARM binary sizes and `.deb` size of the current build.
- Line counts quoted for the deleted C sources (docs 06, 12).
- The on-wire encoding of roxWarp structs: it is inferred from the crate's documented behaviour, not captured on the wire.
- Whether a kernel I2C adapter timeout bounds a stuck bus read.
