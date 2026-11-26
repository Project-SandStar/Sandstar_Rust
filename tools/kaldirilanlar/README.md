# Retired tools (kaldırılanlar)

Files in this folder are **not used, not packaged and not maintained**. They
are kept for reference only. Moved here on 2026-11-25 (S-52).

Do not run them against a device as they are.

| File | Was | Why retired |
|------|-----|-------------|
| `cutover-to-rust.sh` | Switch a device from the C engine to the Rust engine | The C engine no longer exists on the device (checked 2026-11-25). The script has known faults (R-23). |
| `rollback-to-c.sh` | Go back from Rust to the C engine | Same. It did not restore the cutover backup and reported success without checking (R-23). |
| `validate-engines.sh`, `validation-runbook.md` | Compare C and Rust output side by side | There is no C engine to compare with. |
| `sandstar-rust-validate.service` | Read-only Rust service on port 8086 next to the C engine | Same. The package no longer installs it; `postinst` removes an old copy. |
| `deploy-baha.sh`, `deploy-todd.sh`, `installSandstarRust.sh` | Early deploy scripts | Not the deploy path in use (see `docs/Progress/DEPLOYMENT_CHECKLIST.md`). They turn SSH host key checking off and pass the sudo password on the remote command line (R-22), and they carry site-specific connection details (R-24). |

Current tools: `tools/soak-monitor.sh` and `tools/health-monitor.sh` (both
shipped in the package) and the deploy procedure in
`docs/Progress/DEPLOYMENT_CHECKLIST.md`.
