#!/usr/bin/env python3
"""
BASemulator Bridge for Sandstar Rust Engine.

Bridges a real BAScontrol Emulator (BASC22D) by Contemporary Controls
to the Sandstar SimulatorHal REST API.

Input flow:  BASemulator sensor values --> Sandstar /api/sim/inject
Output flow: Sandstar /api/sim/outputs  --> BASemulator HOA force

Logging:
    logs/bridge/       Operational text logs (debug/info)
    logs/data/<session>/
        session.json   Session metadata
        inputs.csv     Every BAS→Sandstar value per cycle
        outputs.csv    Every Sandstar→BAS output per cycle
        channels.csv   Periodic channel snapshots from engine
        bas_raw.csv    Raw BASemulator point dump (all 22 points)

Usage:
    python basemulator-bridge.py                          # Default mapping
    python basemulator-bridge.py --mapping custom.json    # Custom mapping
    python basemulator-bridge.py --interval 0.5           # 500ms poll
    python basemulator-bridge.py --no-data-log            # Skip CSV logging
    python basemulator-bridge.py --dry-run                # Show without writing
    python basemulator-bridge.py --once                   # Single cycle then exit
"""

import argparse
import csv
import json
import logging
import os
import signal
import sys
import time
import xml.etree.ElementTree as ET
from datetime import datetime, timedelta
from pathlib import Path

try:
    import requests
except ImportError:
    print("ERROR: 'requests' library is required.")
    print("Install it with:  pip install requests")
    sys.exit(1)

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------

log = logging.getLogger("bridge")


def setup_logging(log_file: str | None = None, verbose: bool = False):
    level = logging.DEBUG if verbose else logging.INFO
    fmt = "%(asctime)s %(levelname)-5s %(message)s"
    datefmt = "%H:%M:%S"
    handlers: list[logging.Handler] = [logging.StreamHandler(sys.stdout)]
    if log_file:
        Path(log_file).parent.mkdir(parents=True, exist_ok=True)
        handlers.append(logging.FileHandler(log_file, encoding="utf-8"))
    logging.basicConfig(level=level, format=fmt, datefmt=datefmt, handlers=handlers)


# ---------------------------------------------------------------------------
# Data Logger  (structured CSV files per session)
# ---------------------------------------------------------------------------

class DataLogger:
    """Writes structured CSV data files for analysis and charting."""

    def __init__(self, data_dir: str, mapping: dict):
        ts = datetime.now().strftime("%Y%m%d_%H%M%S")
        self.session_dir = Path(data_dir) / f"session_{ts}"
        self.session_dir.mkdir(parents=True, exist_ok=True)
        self.start_time = datetime.now()

        # Write session metadata
        meta = {
            "start_time": self.start_time.isoformat(),
            "basemulator_url": mapping.get("basemulator_url"),
            "sandstar_url": mapping.get("sandstar_url"),
            "poll_interval_s": mapping.get("poll_interval_s", 1.0),
            "input_count": len(mapping.get("inputs", [])),
            "output_count": len(mapping.get("outputs", [])),
            "input_names": [i["bas_name"] for i in mapping.get("inputs", [])],
            "output_names": [o["bas_name"] for o in mapping.get("outputs", [])],
        }
        with open(self.session_dir / "session.json", "w", encoding="utf-8") as f:
            json.dump(meta, f, indent=2)

        # Open CSV writers
        self._inputs_file = open(self.session_dir / "inputs.csv", "w",
                                 newline="", encoding="utf-8")
        self._inputs_csv = csv.writer(self._inputs_file)
        self._inputs_csv.writerow([
            "timestamp", "cycle", "bas_name", "bas_idx", "val_raw", "val_scl",
            "hoa_flg", "sandstar_type", "sandstar_key", "injected_value",
        ])

        self._outputs_file = open(self.session_dir / "outputs.csv", "w",
                                  newline="", encoding="utf-8")
        self._outputs_csv = csv.writer(self._outputs_file)
        self._outputs_csv.writerow([
            "timestamp", "cycle", "bas_name", "bas_idx",
            "sandstar_key_type", "sandstar_key_id", "value",
        ])

        self._channels_file = open(self.session_dir / "channels.csv", "w",
                                   newline="", encoding="utf-8")
        self._channels_csv = csv.writer(self._channels_file)
        self._channels_csv.writerow([
            "timestamp", "cycle", "channel_id", "label", "type",
            "direction", "status", "raw", "cur",
        ])

        self._bas_raw_file = open(self.session_dir / "bas_raw.csv", "w",
                                  newline="", encoding="utf-8")
        self._bas_raw_csv = csv.writer(self._bas_raw_file)
        self._bas_raw_csv.writerow([
            "timestamp", "cycle", "idx", "sts", "val_raw", "val_scl",
            "hoa_flg", "hoa_val", "ws_control", "ws_placed", "mod_control",
        ])

        self._cycle_count = 0
        log.info("Data logging to: %s", self.session_dir)

    def log_bas_raw(self, cycle: int, bas_points: dict[int, dict]):
        """Log raw BASemulator point dump (all 22 points)."""
        ts = datetime.now().isoformat(timespec="milliseconds")
        for idx in sorted(bas_points.keys()):
            p = bas_points[idx]
            self._bas_raw_csv.writerow([
                ts, cycle, idx, p.get("sts", 0),
                p.get("val_raw", "0"), p.get("val_scl", "0"),
                p.get("hoa_flg", 0), p.get("hoa_val", "NULL"),
                p.get("ws_control", 0), p.get("ws_placed", 0),
                p.get("mod_control", 0),
            ])
        self._bas_raw_file.flush()

    def log_input(self, cycle: int, inp_mapping: dict, bas_point: dict,
                  injected_value: float):
        """Log a single input (BAS → Sandstar injection)."""
        ts = datetime.now().isoformat(timespec="milliseconds")
        stype = inp_mapping["sandstar_type"]
        skey = _sandstar_key_str(inp_mapping)
        self._inputs_csv.writerow([
            ts, cycle, inp_mapping["bas_name"], inp_mapping["bas_idx"],
            bas_point.get("val_raw", "0"), bas_point.get("val_scl", "0"),
            bas_point.get("hoa_flg", 0), stype, skey, f"{injected_value:.4f}",
        ])

    def log_output(self, cycle: int, output_mapping: dict, value: float):
        """Log a single output (Sandstar → BAS force)."""
        ts = datetime.now().isoformat(timespec="milliseconds")
        skey_type = output_mapping["sandstar_key_type"]
        skey_id = _output_key_str(output_mapping)
        self._outputs_csv.writerow([
            ts, cycle, output_mapping["bas_name"], output_mapping["bas_idx"],
            skey_type, skey_id, f"{value:.4f}",
        ])

    def log_channels(self, cycle: int, channels: list[dict]):
        """Log engine channel snapshot."""
        ts = datetime.now().isoformat(timespec="milliseconds")
        for ch in channels:
            self._channels_csv.writerow([
                ts, cycle, ch.get("id", "?"), ch.get("label", ""),
                ch.get("type", ""), ch.get("direction", ""),
                ch.get("status", ""), ch.get("raw", 0.0), ch.get("cur", 0.0),
            ])

    def flush(self):
        """Flush all CSV files to disk."""
        self._inputs_file.flush()
        self._outputs_file.flush()
        self._channels_file.flush()

    def close(self, summary: dict | None = None):
        """Close all files and write final session metadata."""
        self._inputs_file.close()
        self._outputs_file.close()
        self._channels_file.close()
        self._bas_raw_file.close()

        # Update session.json with end time and summary
        meta_path = self.session_dir / "session.json"
        with open(meta_path, encoding="utf-8") as f:
            meta = json.load(f)
        meta["end_time"] = datetime.now().isoformat()
        meta["duration_s"] = (datetime.now() - self.start_time).total_seconds()
        if summary:
            meta["summary"] = summary
        with open(meta_path, "w", encoding="utf-8") as f:
            json.dump(meta, f, indent=2)

        log.info("Data session closed: %s", self.session_dir)


def _sandstar_key_str(inp: dict) -> str:
    """Build a human-readable Sandstar key from input mapping."""
    stype = inp["sandstar_type"]
    if stype == "analog":
        return f"dev{inp['sandstar_device']}/addr{inp['sandstar_address']}"
    elif stype == "digital":
        return f"addr{inp['sandstar_address']}"
    elif stype == "i2c":
        return f"dev{inp['sandstar_device']}/0x{inp['sandstar_address']:02x}/{inp.get('sandstar_label', '')}"
    elif stype == "pwm":
        return f"chip{inp.get('sandstar_chip', 0)}/ch{inp.get('sandstar_channel', 0)}"
    return str(inp)


def _output_key_str(om: dict) -> str:
    """Build a human-readable key from output mapping."""
    if om["sandstar_key_type"] == "Digital":
        return f"addr{om.get('sandstar_key_address', '?')}"
    elif om["sandstar_key_type"] == "Pwm":
        return f"chip{om.get('sandstar_key_chip', '?')}/ch{om.get('sandstar_key_channel', '?')}"
    return "?"


# ---------------------------------------------------------------------------
# BASemulator Client  (RDOM XML-RPC over HTTP)
# ---------------------------------------------------------------------------

class BASemulatorClient:
    """Wraps the BASC22D XML-RPC API at /cgi-bin/xml-cgi."""

    def __init__(self, base_url: str, user: str = "admin", password: str = "admin",
                 timeout: float = 5.0):
        self.url = f"{base_url.rstrip('/')}/cgi-bin/xml-cgi"
        self.auth = (user, password)
        self.timeout = timeout

    # -- low-level ----------------------------------------------------------

    def _post_xml(self, xml_body: str) -> ET.Element:
        """POST raw XML and return parsed response Element."""
        resp = requests.post(
            self.url,
            data=xml_body,
            headers={"Content-Type": "application/xml"},
            auth=self.auth,
            timeout=self.timeout,
        )
        resp.raise_for_status()
        return ET.fromstring(resp.text)

    # -- read operations ----------------------------------------------------

    def read_all_points(self) -> dict[int, dict]:
        """Read all 22 points via rd_unit RPC. Returns {idx: {...}}."""
        xml_req = '<rdom fcn="rpc" doc="rtd"><req>rd_unit</req><unit>0</unit></rdom>'
        root = self._post_xml(xml_req)

        points = {}
        for obj in root.iter("obj"):
            idx = int(obj.findtext("idx", "-1"))
            if idx < 0:
                continue
            points[idx] = {
                "idx": idx,
                "sts": int(obj.findtext("sts", "0")),
                "val_raw": obj.findtext("val_raw", "0"),
                "val_scl": obj.findtext("val_scl", "0"),
                "hoa_flg": int(obj.findtext("hoa_flg", "0")),
                "hoa_val": obj.findtext("hoa_val", "NULL"),
                "ws_control": int(obj.findtext("ws_control", "0")),
                "ws_placed": int(obj.findtext("ws_placed", "0")),
                "mod_control": int(obj.findtext("mod_control", "0")),
            }
        return points

    def read_point(self, idx: int) -> dict | None:
        """Read a single point by index."""
        xml_req = f'<rdom fcn="gete" doc="rtd" path="obj[{idx}]" mode="R"/>'
        root = self._post_xml(xml_req)
        obj = root.find(".//obj")
        if obj is None:
            return None
        return {
            "idx": idx,
            "sts": int(obj.findtext("sts", "0")),
            "val_raw": obj.findtext("val_raw", "0"),
            "val_scl": obj.findtext("val_scl", "0"),
            "hoa_flg": int(obj.findtext("hoa_flg", "0")),
            "hoa_val": obj.findtext("hoa_val", "NULL"),
            "ws_control": int(obj.findtext("ws_control", "0")),
            "ws_placed": int(obj.findtext("ws_placed", "0")),
            "mod_control": int(obj.findtext("mod_control", "0")),
        }

    # -- write operations (HOA force) --------------------------------------

    def force_value(self, idx: int, value: float) -> None:
        """Force a point to a specific value via HOA override."""
        self._post_xml(
            f'<rdom fcn="set" doc="rtd" path="obj[{idx}]/hoa_flg">1</rdom>'
        )
        self._post_xml(
            f'<rdom fcn="set" doc="rtd" path="obj[{idx}]/hoa_val">{value}</rdom>'
        )

    def release_force(self, idx: int) -> None:
        """Release HOA override on a point (set hoa_val=NULL)."""
        self._post_xml(
            f'<rdom fcn="set" doc="rtd" path="obj[{idx}]/hoa_val">NULL</rdom>'
        )


# ---------------------------------------------------------------------------
# Sandstar Client  (SimulatorHal REST API)
# ---------------------------------------------------------------------------

class SandstarClient:
    """Wraps the Sandstar SimulatorHal REST endpoints."""

    def __init__(self, base_url: str, timeout: float = 5.0, auth_token: str | None = None):
        self.base_url = base_url.rstrip("/")
        self.timeout = timeout
        self.headers = {}
        if auth_token:
            self.headers["Authorization"] = f"Bearer {auth_token}"

    def inject_points(self, points: list[dict]) -> int:
        """POST /api/sim/inject. Returns number of points injected."""
        resp = requests.post(
            f"{self.base_url}/api/sim/inject",
            json={"points": points},
            headers=self.headers,
            timeout=self.timeout,
        )
        resp.raise_for_status()
        return resp.json().get("injected", 0)

    def read_outputs(self) -> list[dict]:
        """GET /api/sim/outputs. Drains and returns engine writes."""
        resp = requests.get(
            f"{self.base_url}/api/sim/outputs",
            headers=self.headers,
            timeout=self.timeout,
        )
        resp.raise_for_status()
        return resp.json().get("outputs", [])

    def read_channels(self) -> list[dict]:
        """GET /api/read — return all channel data."""
        resp = requests.get(
            f"{self.base_url}/api/read?filter=point",
            headers=self.headers,
            timeout=self.timeout,
        )
        resp.raise_for_status()
        ct = resp.headers.get("content-type", "")
        return resp.json() if ct.startswith("application/json") else []


# ---------------------------------------------------------------------------
# Mapping helpers
# ---------------------------------------------------------------------------

def load_mapping(path: str) -> dict:
    with open(path, encoding="utf-8") as f:
        return json.load(f)


def _safe_float(val: str, default: float = 0.0) -> float:
    """Parse a float from a BAS point value string, handling 'NULL' etc."""
    try:
        return float(val)
    except (ValueError, TypeError):
        return default


def build_inject_point(inp: dict, bas_point: dict) -> dict:
    """Build a Sandstar inject-point dict from a mapping entry + BAS data."""
    val_scl = _safe_float(bas_point.get("val_scl", "0"))
    stype = inp["sandstar_type"]

    if stype == "analog":
        return {
            "type": "analog",
            "device": inp["sandstar_device"],
            "address": inp["sandstar_address"],
            "value": val_scl,
        }
    elif stype == "digital":
        return {
            "type": "digital",
            "address": inp["sandstar_address"],
            "value": val_scl != 0.0,
        }
    elif stype == "i2c":
        return {
            "type": "i2c",
            "device": inp["sandstar_device"],
            "address": inp["sandstar_address"],
            "label": inp.get("sandstar_label", ""),
            "value": val_scl,
        }
    elif stype == "pwm":
        return {
            "type": "pwm",
            "chip": inp.get("sandstar_chip", 0),
            "channel": inp.get("sandstar_channel", 0),
            "value": val_scl,
        }
    else:
        raise ValueError(f"Unknown sandstar_type '{stype}' in mapping")


def match_output_to_mapping(output: dict, output_mappings: list[dict]) -> dict | None:
    """Find the mapping entry that matches a Sandstar output WriteKey."""
    key = output.get("key", {})
    key_type = key.get("type", "")
    for om in output_mappings:
        if om["sandstar_key_type"] == key_type:
            if key_type == "Digital" and key.get("address") == om.get("sandstar_key_address"):
                return om
            if key_type == "Pwm" and key.get("chip") == om.get("sandstar_key_chip") and \
               key.get("channel") == om.get("sandstar_key_channel"):
                return om
    return None


# ---------------------------------------------------------------------------
# Status display
# ---------------------------------------------------------------------------

def format_status_line(cycle: int, input_values: dict[str, float],
                       output_values: dict[str, float]) -> str:
    """Build compact one-line status."""
    now = datetime.now().strftime("%H:%M:%S")
    parts_in = " ".join(f"{_short_name(k)}={v:.1f}" for k, v in input_values.items())
    parts_out = " ".join(f"{_short_name(k)}={v:.2g}" for k, v in output_values.items())
    return f"[{now}] Cycle {cycle} | IN: {parts_in} | OUT: {parts_out}"


def _short_name(name: str) -> str:
    """Extract short point name like 'UI1' from 'UI1 Zone Temp'."""
    return name.split()[0] if " " in name else name


# ---------------------------------------------------------------------------
# Bridge
# ---------------------------------------------------------------------------

class Bridge:
    """Main bridge loop connecting BASemulator <-> Sandstar."""

    def __init__(self, mapping: dict, dry_run: bool = False,
                 data_logger: DataLogger | None = None):
        bas_url = mapping.get("basemulator_url", "http://localhost:5001")
        bas_auth = mapping.get("basemulator_auth", {})
        ss_url = mapping.get("sandstar_url", "http://localhost:8085")
        self.interval = mapping.get("poll_interval_s", 1.0)
        self.input_map = mapping.get("inputs", [])
        self.output_map = mapping.get("outputs", [])
        self.dry_run = dry_run
        self.data_log = data_logger

        self.bas = BASemulatorClient(
            bas_url,
            user=bas_auth.get("user", "admin"),
            password=bas_auth.get("pass", "admin"),
        )
        self.ss = SandstarClient(
            ss_url,
            auth_token=mapping.get("sandstar_auth_token"),
        )

        self.cycle = 0
        self.errors = 0
        self.start_time = time.monotonic()
        self._forced_outputs: set[int] = set()
        # Channel snapshot interval (every N cycles)
        self._channel_snapshot_interval = 10
        self._total_inputs = 0
        self._total_outputs = 0

    # -- single cycle -------------------------------------------------------

    def run_cycle(self) -> tuple[dict[str, float], dict[str, float]]:
        """Execute one input+output bridge cycle. Returns (inputs, outputs)."""
        input_values: dict[str, float] = {}
        output_values: dict[str, float] = {}

        # ---- Input flow: BASemulator -> Sandstar ----
        try:
            bas_points = self.bas.read_all_points()
        except Exception as e:
            log.error("BASemulator read failed: %s", e)
            self.errors += 1
            bas_points = {}

        if bas_points:
            # Log raw BAS dump
            if self.data_log:
                self.data_log.log_bas_raw(self.cycle, bas_points)

            inject_batch = []
            for inp in self.input_map:
                idx = inp["bas_idx"]
                bp = bas_points.get(idx)
                if bp is None:
                    log.debug("BAS idx %d not found in response", idx)
                    continue
                val = _safe_float(bp["val_scl"])
                input_values[inp["bas_name"]] = val
                inject_point = build_inject_point(inp, bp)
                inject_batch.append(inject_point)

                # Log to CSV
                if self.data_log:
                    self.data_log.log_input(self.cycle, inp, bp, val)

                log.debug("%s: %.3f -> Sandstar %s(%s)",
                          inp["bas_name"], val,
                          inp["sandstar_type"],
                          inp.get("sandstar_address", inp.get("sandstar_device", "?")))

            if inject_batch and not self.dry_run:
                try:
                    n = self.ss.inject_points(inject_batch)
                    self._total_inputs += n
                    log.debug("Injected %d points into Sandstar", n)
                except Exception as e:
                    log.error("Sandstar inject failed: %s", e)
                    self.errors += 1
            elif inject_batch and self.dry_run:
                log.info("DRY-RUN: Would inject %d points: %s",
                         len(inject_batch), json.dumps(inject_batch, indent=None))

        # ---- Output flow: Sandstar -> BASemulator ----
        try:
            outputs = self.ss.read_outputs() if not self.dry_run else []
        except Exception as e:
            log.error("Sandstar outputs read failed: %s", e)
            self.errors += 1
            outputs = []

        for out in outputs:
            om = match_output_to_mapping(out, self.output_map)
            if om is None:
                log.debug("No mapping for output key: %s", out.get("key"))
                continue
            bas_idx = om["bas_idx"]
            value = out["value"]
            output_values[om["bas_name"]] = value
            self._total_outputs += 1

            # Log to CSV
            if self.data_log:
                self.data_log.log_output(self.cycle, om, value)

            if not self.dry_run:
                try:
                    self.bas.force_value(bas_idx, value)
                    self._forced_outputs.add(bas_idx)
                    log.debug("Sandstar %s -> %s (idx %d): %.3f",
                              om["sandstar_key_type"], om["bas_name"], bas_idx, value)
                except Exception as e:
                    log.error("BASemulator force idx %d failed: %s", bas_idx, e)
                    self.errors += 1
            else:
                log.info("DRY-RUN: Would force BAS idx %d (%s) to %.3f",
                         bas_idx, om["bas_name"], value)

        # ---- Periodic channel snapshot ----
        if (self.data_log and not self.dry_run and
                self.cycle % self._channel_snapshot_interval == 0):
            try:
                channels = self.ss.read_channels()
                self.data_log.log_channels(self.cycle, channels)
            except Exception as e:
                log.debug("Channel snapshot failed: %s", e)

        # Flush data logs periodically
        if self.data_log and self.cycle % 5 == 0:
            self.data_log.flush()

        self.cycle += 1
        return input_values, output_values

    # -- cleanup ------------------------------------------------------------

    def release_all_forces(self):
        """Release HOA overrides on all outputs we've forced."""
        if not self._forced_outputs:
            return
        log.info("Releasing %d forced outputs on BASemulator...", len(self._forced_outputs))
        for idx in self._forced_outputs:
            try:
                self.bas.release_force(idx)
                log.debug("Released force on idx %d", idx)
            except Exception as e:
                log.warning("Failed to release idx %d: %s", idx, e)
        self._forced_outputs.clear()

    def get_summary(self) -> dict:
        elapsed = time.monotonic() - self.start_time
        return {
            "cycles": self.cycle,
            "duration_s": round(elapsed, 1),
            "errors": self.errors,
            "total_inputs_injected": self._total_inputs,
            "total_outputs_written": self._total_outputs,
        }

    def print_summary(self):
        s = self.get_summary()
        dur = str(timedelta(seconds=int(s["duration_s"])))
        log.info("--- Bridge Summary ---")
        log.info("Cycles: %d  |  Duration: %s  |  Errors: %d",
                 s["cycles"], dur, s["errors"])
        log.info("Total injected: %d  |  Total outputs: %d",
                 s["total_inputs_injected"], s["total_outputs_written"])


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description="BASemulator Bridge for Sandstar Rust Engine (BASC22D)")
    parser.add_argument("--mapping", default=None,
                        help="Path to mapping JSON (default: tools/basemulator-mapping.json)")
    parser.add_argument("--interval", type=float, default=None,
                        help="Poll interval in seconds (overrides mapping file)")
    parser.add_argument("--log-file", default=None,
                        help="Also log to this file")
    parser.add_argument("--data-dir", default=None,
                        help="Data log directory (default: logs/data/)")
    parser.add_argument("--no-data-log", action="store_true",
                        help="Disable structured CSV data logging")
    parser.add_argument("--dry-run", action="store_true",
                        help="Show what would happen without writing to either side")
    parser.add_argument("--once", action="store_true",
                        help="Run a single cycle then exit")
    parser.add_argument("--verbose", "-v", action="store_true",
                        help="Enable debug logging")
    args = parser.parse_args()

    setup_logging(log_file=args.log_file, verbose=args.verbose)

    # Resolve mapping path
    if args.mapping:
        mapping_path = args.mapping
    else:
        mapping_path = str(Path(__file__).parent / "basemulator-mapping.json")

    if not Path(mapping_path).is_file():
        log.error("Mapping file not found: %s", mapping_path)
        sys.exit(1)

    mapping = load_mapping(mapping_path)
    if args.interval is not None:
        mapping["poll_interval_s"] = args.interval

    # Resolve data log directory
    if args.data_dir:
        data_dir = args.data_dir
    else:
        data_dir = str(Path(__file__).parent.parent / "logs" / "data")

    log.info("BASemulator Bridge starting")
    log.info("  BASemulator: %s", mapping.get("basemulator_url"))
    log.info("  Sandstar:    %s", mapping.get("sandstar_url"))
    log.info("  Inputs:      %d mappings", len(mapping.get("inputs", [])))
    log.info("  Outputs:     %d mappings", len(mapping.get("outputs", [])))
    log.info("  Interval:    %.1fs", mapping.get("poll_interval_s", 1.0))
    if args.dry_run:
        log.info("  Mode:        DRY-RUN (no writes)")

    # Data logger
    data_logger = None
    if not args.no_data_log and not args.dry_run:
        data_logger = DataLogger(data_dir, mapping)
        log.info("  Data log:    %s", data_logger.session_dir)
    else:
        log.info("  Data log:    disabled")

    bridge = Bridge(mapping, dry_run=args.dry_run, data_logger=data_logger)

    # Graceful shutdown
    shutdown_requested = False

    def on_signal(signum, frame):
        nonlocal shutdown_requested
        if shutdown_requested:
            log.warning("Force exit")
            sys.exit(1)
        shutdown_requested = True
        log.info("\nShutdown requested (Ctrl+C)...")

    signal.signal(signal.SIGINT, on_signal)
    if hasattr(signal, "SIGTERM"):
        signal.signal(signal.SIGTERM, on_signal)

    # Connection check
    log.info("Testing connections...")
    try:
        pts = bridge.bas.read_all_points()
        log.info("  BASemulator: OK (%d points)", len(pts))
    except Exception as e:
        log.warning("  BASemulator: FAILED (%s) -- will retry in loop", e)

    if not args.dry_run:
        try:
            bridge.ss.read_outputs()
            log.info("  Sandstar:    OK")
        except Exception as e:
            log.warning("  Sandstar:    FAILED (%s) -- will retry in loop", e)

    log.info("Bridge running. Press Ctrl+C to stop.")

    try:
        while not shutdown_requested:
            in_vals, out_vals = bridge.run_cycle()
            status = format_status_line(bridge.cycle, in_vals, out_vals)
            log.info(status)

            if args.once:
                break

            # Sleep in small increments so we can respond to Ctrl+C quickly
            deadline = time.monotonic() + bridge.interval
            while time.monotonic() < deadline and not shutdown_requested:
                time.sleep(min(0.1, deadline - time.monotonic()))
    except Exception as e:
        log.error("Unexpected error: %s", e)
    finally:
        if not args.dry_run:
            bridge.release_all_forces()
        bridge.print_summary()
        if data_logger:
            data_logger.close(summary=bridge.get_summary())


if __name__ == "__main__":
    main()
