# Sandstar Rust — Deploy and Rollback Runbook

**Rewritten:** 2026-11-18. This replaces the 2026-03 checklist, which covered a Linux VM build, a jump-host flow and C side-by-side validation; that version remains in git history.
**Applies to:** `sandstar` `.deb` 2.9.x on a BeagleBone (Debian 9, kernel 4.9, armv7, systemd).

> **No site data in this file.** Host addresses, SSH users and ports, jump hosts, connection names and passwords live only in the machine-local connection files used by `tools/installSandstar.sh`. Those files are git-ignored. Examples below use placeholders such as `<device>` and `<hash>`.

---

## 0. Approval and scope

- **Approval:** a deploy, service restart, rollback or any write on the device needs explicit approval from the project owner. In one working session, approval covers read-only commands and writes inside the approved deploy (backup, install, restart). It does not cover new kinds of writes: device settings, GPIO/sysfs, package removal, rollback, deletion.
- **Never test against real equipment:** no BACnet/MQTT write tests against real equipment. Use `tools/bacnet_sim.py`, a local mosquitto or `cargo sim-run`.
- **Watchdog:** the board has an external TPL5010 watchdog on GPIO60, kicked by the engine. Keep the time the service is stopped short. If the watchdog is active, a long stop can reset the board.

## 1. Prerequisites (Windows build host)

| Need | Notes |
|---|---|
| Rust toolchain | Pinned by `rust-toolchain.toml` (1.99.1) |
| Cross environment | Machine-local `arm-env` script (PowerShell or Git Bash), which sets `CC`/`AR` for armv7. The linker is a Zig wrapper targeting glibc 2.24 (`.cargo/config.toml`). |
| `cargo-deb` | For `cargo arm-deb` |
| `shaystack` checkout | Sibling of the repo. The `.deb` bundles 16 lookup tables from it (`crates/sandstar-server/Cargo.toml`, `[package.metadata.deb]`). |
| MSYS2 with `expect` and `rsync` | `tools/installSandstar.sh` runs under MSYS2 bash |
| Connection file for the target | Machine-local and git-ignored. Never print its contents. |

## 2. Pre-flight

1. The branch is merged to `master` and CI on `master` is green.
2. `bash tools/gates.sh` prints all gates green on the commit you will build.
3. Note the short commit hash `<hash>`; it is embedded in the binary and shown by `/api/about`.

## 3. Build and check the package

```bash
source /c/czb/arm-env.sh        # or: . C:\czb\arm-env.ps1  (machine-local path)
cargo arm-build-svm
cargo arm-deb --no-strip
```

- The `.deb` is written under `target/armv7-unknown-linux-gnueabihf/debian/`.
- Check it with `dpkg-deb -I <deb>` (version, maintainer scripts) and `dpkg-deb -c <deb>` (binaries, unit files, `initialize.sh`, 16 tables).

**Package facts that matter for a deploy:**
- **`prerm`** stops `sandstar-engine`.
- **`postinst`:**
  - creates the directories;
  - runs `chown -R eacio:root /home/eacio/sandstar/`, `daemon-reload` and `enable`;
  - removes an old `/etc/systemd/system/sandstar-rust-validate.service` (retired in S-52) unless that unit is running;
  - **starts or restarts the engine itself**. `dpkg -i` alone brings the service back up.
- **`postrm purge` deletes `/var/log/sandstar`.** Never use `dpkg -P` / `--purge` on a device whose logs matter.
- **Conffiles:** `sandstar-engine.service`, `initialize.sh`, `/etc/logrotate.d/sandstar`. If the device copy differs from the packaged one, `dpkg -i` asks what to do. Compare first (§4).
- **Not package-owned** (survive upgrades; must be in the backup):
  - systemd drop-ins under `/etc/systemd/system/sandstar-engine.service.d/` (driver configs, file-root);
  - `/etc/sandstar`;
  - site configuration under `/home/eacio/sandstar/etc/`;
  - any device-only table files.
- **Monitoring scripts in the package (S-52):** `soak-monitor.sh` (run by hand for a soak; checks the `sandstar-engine` unit, `/health` on 8085, RSS, restarts (from changes of the unit start time: systemd 232 has no `NRestarts`) and new `ERROR` lines in the engine log file (the service user cannot read the journal); exit code 1 if any alert) and `health-monitor.sh` (cron, one line every 5 minutes: `OK fault= down= overruns= poll_ms= mem_kb= max_poll_ms=`, or `CRITICAL engine not healthy` / `WARNING diagnostics unreadable`). The C-era scripts (`cutover-to-rust.sh`, `rollback-to-c.sh`, `validate-engines.sh`) and the `sandstar-rust-validate` unit are no longer shipped; an upgrade removes them from the device. They are kept, unused, in `tools/kaldirilanlar/`.

## 4. Inspect the device (read-only)

- Installed version: `dpkg -s sandstar`, `curl -s http://localhost:8085/api/about` (`buildInfo`).
- Service: `systemctl status sandstar-engine`, `systemctl cat sandstar-engine` (shows drop-ins).
- Conffiles: compare `md5sum` of the four conffiles on the device with the files in `etc/` of the commit you build. Decide in advance whether to keep or replace each one that differs.
- Disk space: `df -h /home /var/log`.

## 5. Backup (mandatory, before every deploy)

On the device, create `~/sandstar-backups/<YYYY-MM-DD>_pre-<hash>/` containing:

| File | Content |
|---|---|
| `sandstar-home.tar.gz` | `/home/eacio/sandstar`, with permissions preserved (`tar -cpzf`) |
| `system-files.tar.gz` | The unit files and their drop-in directories, `/etc/sandstar`, `/etc/logrotate.d/sandstar` |
| `ROLLBACK_<installed-version>.deb` | A `.deb` whose binary md5 matches the **installed** binary (find it among earlier packages by md5) |
| `info.txt` | `dpkg -s sandstar`, `/api/about`, `systemctl cat sandstar-engine`, md5 of the installed binaries |
| `SHA256SUMS` | `sha256sum` of every file above |

Then copy the folder to the machine-local backup area on the build host and run `sha256sum -c SHA256SUMS` there. Do not continue until both copies verify.

If the deploy tooling touches any other directory, back that up too. Read every step of the script first.

## 6. Deploy

From MSYS2 bash, in `tools/`:

```bash
./installSandstar.sh <connection-name>
```

The script copies the package, then stops the service, installs it with `dpkg -i` and starts it again. Its output can echo connection details: show only what you need (filter for status lines) and never paste raw output into docs or tickets.

**Manual equivalent:**
1. Copy the `.deb` to the device.
2. `sudo dpkg -i <deb>`. `prerm` stops the service and `postinst` restarts it.
3. `systemctl status sandstar-engine`.

Do **not** run a recursive `chmod`/`chown` of your own; `postinst` already sets ownership. An extra recursive `chmod -R 755` once broke file permissions on a device.

## 7. Verify

Read the journal soon after the start: it is volatile and holds well under an hour.

1. **Build:** `curl -s http://localhost:8085/api/about` shows `2.9.x` with the new `<hash>`.
   - `curl -sf http://localhost:8085/health` must succeed. Since S-21 it returns 503 (and `curl -f` fails) when the engine does not answer within 2 s; `tools/health-monitor.sh` and `tools/soak-monitor.sh` rely on this.
2. **Unit:** `systemctl status sandstar-engine` is `active (running)`. The device's systemd has no `NRestarts` property, so use `systemctl show -p ActiveEnterTimestamp` instead.
3. **`initialize.sh` output** (`journalctl -u sandstar-engine -b`, needs `sudo`): one GPIO60 line: `already exported`, `exported (attempt n)` or `export failed … ; pinmux`. It never fails the start.
4. **Engine log** `/var/log/sandstar/sandstar-engine.log.<UTC-date>`:
   - `tables loaded` with the expected count (17 on device 1-6), and `loaded tables … skipped=0`. From S-56 on, each table that was left out has its own `table not loaded at start-up` error line with the table name and the reason; the service still starts, and a channel whose `database.zinc` row names that table reads as fault and refuses writes of engineering values;
   - a watchdog line saying GPIO60 was opened, or why not (the engine retries for about 5 minutes);
   - no repeated `poll cycle exceeded`;
   - from S-43 on, a stop or restart ends the previous run with `shutdown signal received (SIGTERM)`, `engine stopped … reason="SIGTERM"` and `watchdog: /dev/watchdog closed (magic close)` when `/dev/watchdog` was open. On the first deploy of S-43 the old binary is stopped, so these lines appear from the next restart on. `magic close failed`, `HAL shutdown skipped` or `exiting without waiting for the stuck poll` need a look;
   - with `--cluster` and no mTLS certificates (the packaged unit): `roxWarp: no mTLS configured, cluster listener disabled` and `cluster listener not started`, so port 7443 is not listening (S-36). With certificates expect `starting mTLS listener`; `mTLS config failed` means the certificate files need fixing.
5. **Warnings:** count ERROR and WARN lines from the start time and compare them with the known types for this device (for example BACnet `comm fault` when a BACnet drop-in is active). Investigate anything new.
6. **Data:** `curl -s http://localhost:8085/api/status` and spot-read a few channels with `/api/read`.
7. **Next day:** confirm that log pruning kept at most 14 dated files in `/var/log/sandstar/`.

## 7a. Turning REST auth on or off

The packaged unit starts with no auth. Auth is a device-local setting: an
`EnvironmentFile` drop-in, so the password is not in the unit file or in
`systemctl show`. Needs explicit approval (new kind of device write). Back up
`/etc/systemd/system/sandstar-engine.service.d` and `/etc/sandstar` first.

1. `/etc/sandstar/auth.env`, owner `root:root`, mode `0600` (write it with
   `umask 077`; never print it):
   - `SANDSTAR_AUTH_USER=<user>` and `SANDSTAR_AUTH_PASS=<password>`: SCRAM login (editor);
   - optional `SANDSTAR_AUTH_TOKEN=<token>`: bearer for scripts;
   - `SANDSTAR_WS_ALLOWED_ORIGINS=<origin>[,...]`: page origins allowed besides
     the device itself, e.g. the editor dev server `http://localhost:8091`.
2. `/etc/systemd/system/sandstar-engine.service.d/auth.conf` (0644):
   `[Service]` / `EnvironmentFile=/etc/sandstar/auth.env`.
3. `systemctl daemon-reload && systemctl restart sandstar-engine`.
4. Verify on the device (no credentials, no real write):
   - `/api/about`, `/api/read`, `/health`, `/api/cluster/status`: 200;
   - `POST /api/pointWrite`, `HEAD /live/sedonaState`, `GET /api/sox/tree`,
     `PUT /file/x`: 401;
   - log: one `SCRAM-SHA-256 auth configured` line, no new ERROR;
   - `systemctl show sandstar-engine -p Environment` does not contain the password;
   - the editor logs in (`bash tools/editor-dev.sh`) and loads the tree.
5. Off: remove both files, `daemon-reload`, restart.

Clients that write without credentials get 401 once auth is on. Reads listed
above stay open. `health-monitor.sh` (cron) only reads `/health` and
`/api/diagnostics`.

## 8. Rollback

Rolling back is a write outside the deploy scope and needs its own approval.

1. `sudo dpkg -i ~/sandstar-backups/<folder>/ROLLBACK_<version>.deb`. `postinst` restarts the service.
2. If permissions or site files were damaged, restore them from `sandstar-home.tar.gz` (`sudo tar -xpzf … -C /`) and, if needed, `system-files.tar.gz`. Then run `sudo systemctl daemon-reload` and restart the service.
3. Verify as in §7.

## 9. After the deploy

- Record the deploy in the internal progress notes: the package hash, the backup folder name and the verification evidence (log lines, `/api/about`).
- A risk or fix moves to "on device" only after the deploy, and to "verified in the field" only with evidence from the device.
- Schedule any next-day checks (log pruning, watchdog behaviour).

## Reference: paths on the device

| Path | What |
|---|---|
| `/home/eacio/sandstar/bin/` | `sandstar-engine-server`, `sandstar-cli` |
| `/home/eacio/sandstar/etc/EacIo/` | Config directory (`--config-dir`) |
| `/home/eacio/sandstar/etc/config/` | Lookup tables |
| `/home/eacio/sandstar/etc/init/initialize.sh` | `ExecStartPre` hardware init (conffile) |
| `/etc/systemd/system/sandstar-engine.service` | Unit (conffile); drop-ins in `sandstar-engine.service.d/` |
| `/var/log/sandstar/sandstar-engine.log.YYYY-MM-DD` | Daily engine log, newest 14 kept |
| `/var/run/sandstar/sandstar-engine.pid` | PID file |
| `/tmp/sandstar-engine.sock` | CLI IPC socket (Rust default) |
