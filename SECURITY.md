# Security policy

## Supported versions

Sandstar is deployed as a single-binary `.deb` to embedded BeagleBone devices. Security fixes are applied to the latest released minor version; older versions may remain running on production hardware but are not patched in-place — upgrade is the supported recovery path.

| Version | Supported |
|---------|-----------|
| 2.8.x   | ✅ Latest; all security fixes land here |
| 2.0.x – 2.7.x | ❌ No backports; upgrade to 2.8.x |
| 1.x     | ❌ End of life (pre-`csrc/` removal) |

## What counts as a security issue

- Remote code execution, memory-safety bugs in the shipped Rust binary, or ways to bypass the auth middleware without valid credentials
- Denial-of-service attacks that the existing rate limiter does not mitigate (the documented limit is 100 req/s by default)
- Information disclosure (config leakage, cross-tenant data bleed, timing attacks on the SCRAM challenge)
- Network-side protocol attacks (SOX/DASP, roxWarp, BACnet/IP, MQTT) that lead to priority-array override, actuator drive, or credential exposure
- Privilege escalation through the installed systemd unit or the deploy script

Issues in **research code** (not yet deployed, marked "Not started" or `#[ignore]` in the roadmap) are welcome bug reports but are not treated as security issues until the code reaches production.

## Reporting

Open a GitHub issue on <https://github.com/Project-SandStar/Sandstar_Rust> and tag it `security`. For issues that should not be public until a fix ships, contact the repository maintainer directly; issues are acknowledged within a few business days and an initial assessment is shared within a week.

Please include:

- Affected version (output of `/api/about` or `dpkg -l | grep sandstar` on the device)
- Reproduction steps or PoC
- What access level is assumed (network-adjacent, local LAN, authenticated user, unauthenticated)
- Your expected impact

## Response

1. Acknowledge — within a few business days.
2. Triage — we reproduce and classify severity (informational / low / medium / high / critical).
3. Fix — for accepted issues, a patch lands on master with a CHANGELOG entry. Timing depends on severity: critical issues typically patched within a week, lower-severity fixes folded into the next minor release.
4. Credit — with your permission, the reporter is credited in the CHANGELOG entry.

## Hardening defaults (production baseline)

Every deployment should at minimum:

- Run with `--http-bind 127.0.0.1` (default) behind a reverse proxy; expose HTTP externally only when TLS + auth are configured.
- Set `--auth-token` **or** `--auth-user`/`--auth-pass` (SCRAM). Bearer-only is accepted but SCRAM is preferred on modern deployments.
- Leave `--rate-limit` at the default 100 req/s unless there is a specific reason to widen it.
- For cluster nodes: populate `ClusterConfig.{cert_path, key_path, ca_path}` to require mTLS. Plain WS cluster traffic is allowed only for local-testing setups.
- Keep `dpkg` updated to apply the latest minor version.

See `README.md` §"Security" for the full list of middleware in force.

## Cryptography notes

- TLS: `rustls` 0.23 with the `ring` provider.
- SCRAM: RFC 5802 with SHA-256 + PBKDF2 (default iteration count in `auth.rs`; configurable per-user).
- No OpenSSL link. The production binary is statically linked (no `.so` dependencies); see `ldd /home/eacio/sandstar/bin/sandstar-engine-server` on a deployed device — "not a dynamic executable".

## What's not in scope

- Physical security of the BeagleBone (local serial console, bootloader tampering).
- Supply-chain security of the build host itself (we use `cargo audit` in CI; report compromised dependency advisories through that channel).
- Denial-of-service via saturating the serial/I2C/GPIO hardware at the electrical layer.
