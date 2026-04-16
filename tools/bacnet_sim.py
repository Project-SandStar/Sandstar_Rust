#!/usr/bin/env python3
"""Minimal hand-crafted BACnet/IP device simulator.

Responds to:
  - Who-Is (broadcast or unicast) with I-Am for device 12345
  - ReadProperty(Device, 12345, ObjectList=76) with list of [AI-0, AI-1, BI-0]
  - ReadProperty(*, *, ObjectName=77) with hardcoded names
  - ReadProperty(*, *, PresentValue=85) with hardcoded values

Does NOT depend on bacpypes3. Hand-crafts all frames per the ASHRAE 135 spec.

Usage: py tools/bacnet_sim.py [bind_ip]
"""

import socket
import struct
import sys

DEVICE_INSTANCE = 12345
VENDOR_ID = 999

# Objects we advertise: (object_type, instance, name, value_encoder)
OBJECTS = [
    (0, 0, "TempSensor",     lambda: encode_real(72.5)),
    (0, 1, "HumiditySensor", lambda: encode_real(45.0)),
    (3, 0, "Occupancy",      lambda: encode_enum(1)),  # BI: active=1
]

def encode_obj_id(obj_type, instance):
    """BACnet ObjectIdentifier: tag 12, LVT=4, 4 bytes = (type << 22) | instance."""
    raw = ((obj_type & 0x3FF) << 22) | (instance & 0x3F_FFFF)
    return bytes([0xC4]) + raw.to_bytes(4, "big")

def encode_unsigned(n):
    if n <= 0xFF:
        return bytes([0x21, n])
    if n <= 0xFFFF:
        return bytes([0x22]) + n.to_bytes(2, "big")
    return bytes([0x24]) + n.to_bytes(4, "big")

def encode_real(f):
    return bytes([0x44]) + struct.pack(">f", f)

def encode_enum(v):
    if v <= 0xFF:
        return bytes([0x91, v])
    return bytes([0x92]) + v.to_bytes(2, "big")

def encode_char_string(s):
    raw = s.encode("utf-8")
    body = bytes([0x00]) + raw  # charset 0 = UTF-8
    n = len(body)
    if n <= 4:
        return bytes([0x70 | n]) + body
    if n <= 253:
        return bytes([0x75, n]) + body
    return bytes([0x76]) + n.to_bytes(2, "big") + body

def encode_ctx_unsigned(tag_num, val):
    if val <= 0xFF:
        return bytes([(tag_num << 4) | 0x08 | 1, val])
    if val <= 0xFFFF:
        return bytes([(tag_num << 4) | 0x08 | 2]) + val.to_bytes(2, "big")
    return bytes([(tag_num << 4) | 0x08 | 4]) + val.to_bytes(4, "big")

def build_bvll(bvll_type, apdu):
    """BVLL header (4 bytes) + NPDU (2 bytes) + APDU."""
    total_len = 4 + 2 + len(apdu)
    return bytes([0x81, bvll_type]) + total_len.to_bytes(2, "big") + bytes([0x01, 0x00]) + apdu

def make_i_am():
    """I-Am unconfirmed request APDU + BVLL unicast wrap."""
    apdu = bytes([0x10, 0x00])  # Unconfirmed Request, service I-Am
    apdu += encode_obj_id(8, DEVICE_INSTANCE)  # Device object
    apdu += encode_unsigned(1476)              # Max-APDU
    apdu += bytes([0x91, 0x00])                # Segmentation = segmentedBoth (0)... actually 3=noSegmentation
    apdu = apdu[:-1] + bytes([0x03])           # set segmentation = noSegmentation
    apdu += encode_unsigned(VENDOR_ID)
    return build_bvll(0x0A, apdu)  # BVLL unicast

def parse_confirmed_request(apdu):
    """Parse a Confirmed-Request APDU header.
    Returns (invoke_id, service, payload_offset) or None.
    """
    if len(apdu) < 4 or (apdu[0] & 0xF0) != 0x00:
        return None
    return (apdu[2], apdu[3], 4)

def parse_read_property(apdu):
    """Parse a Confirmed-Request ReadProperty APDU.
    Returns (invoke_id, object_type, instance, property_id, array_index) or None.
    """
    if len(apdu) < 4 or (apdu[0] & 0xF0) != 0x00:
        return None
    invoke_id = apdu[2]
    service = apdu[3]
    if service != 0x0C:  # ReadProperty
        return None
    pos = 4
    # Context tag 0: ObjectIdentifier (0x0C, 4 bytes)
    if apdu[pos] != 0x0C:
        return None
    raw_id = int.from_bytes(apdu[pos+1:pos+5], "big")
    obj_type = (raw_id >> 22) & 0x3FF
    instance = raw_id & 0x3F_FFFF
    pos += 5
    # Context tag 1: PropertyIdentifier
    tag = apdu[pos]
    if tag & 0xF0 != 0x10:  # tag num 1
        return None
    lvt = tag & 0x07
    pos += 1
    prop_id = int.from_bytes(apdu[pos:pos+lvt], "big")
    pos += lvt
    # Optional context tag 2: ArrayIndex
    array_index = None
    if pos < len(apdu) and (apdu[pos] & 0xF0) == 0x20:
        lvt2 = apdu[pos] & 0x07
        pos += 1
        array_index = int.from_bytes(apdu[pos:pos+lvt2], "big")
    return (invoke_id, obj_type, instance, prop_id, array_index)

def make_read_property_ack(invoke_id, obj_type, instance, prop_id, value_bytes):
    """Build a Complex-ACK ReadProperty response APDU + BVLL unicast wrap."""
    apdu = bytes([0x30, invoke_id, 0x0C])  # Complex-ACK, invoke_id, ReadProperty
    # Context 0: ObjectIdentifier
    raw_id = ((obj_type & 0x3FF) << 22) | (instance & 0x3F_FFFF)
    apdu += bytes([0x0C]) + raw_id.to_bytes(4, "big")
    # Context 1: PropertyIdentifier
    apdu += encode_ctx_unsigned(1, prop_id)
    # Context 3: property-value (opening)
    apdu += bytes([0x3E])
    apdu += value_bytes
    apdu += bytes([0x3F])
    return build_bvll(0x0A, apdu)

def make_object_list_ack(invoke_id, device_instance):
    """ObjectList value = all 3 objects + the Device itself."""
    values = encode_obj_id(8, device_instance)  # Include Device
    for (ot, inst, _, _) in OBJECTS:
        values += encode_obj_id(ot, inst)
    return make_read_property_ack(invoke_id, 8, device_instance, 76, values)

def make_object_name_ack(invoke_id, obj_type, instance):
    for (ot, inst, name, _) in OBJECTS:
        if ot == obj_type and inst == instance:
            return make_read_property_ack(invoke_id, obj_type, instance, 77, encode_char_string(name))
    return None

def make_present_value_ack(invoke_id, obj_type, instance):
    for (ot, inst, _, value_fn) in OBJECTS:
        if ot == obj_type and inst == instance:
            return make_read_property_ack(invoke_id, obj_type, instance, 85, value_fn())
    return None

# ── ReadPropertyMultiple (Phase B9) ────────────────────────────────────────

def encode_rpm_ack(invoke_id, results):
    """Encode a ReadPropertyMultiple-ACK.
    `results` is a list of (obj_type, instance, property_id, value_bytes).
    `value_bytes` is a pre-encoded application-tagged BacnetValue.
    """
    apdu = bytes([0x30, invoke_id, 0x0E])  # Complex-ACK, invoke, service=RPM
    for (ot, inst, pid, value_bytes) in results:
        # Context 0: ObjectIdentifier
        raw_id = ((ot & 0x3FF) << 22) | (inst & 0x3F_FFFF)
        apdu += bytes([0x0C]) + raw_id.to_bytes(4, "big")
        # Context 1: listOfResults opening
        apdu += bytes([0x1E])
        # Context 2: PropertyIdentifier (1-byte for common props)
        if pid <= 0xFF:
            apdu += bytes([0x29, pid])
        else:
            apdu += bytes([0x2A]) + pid.to_bytes(2, "big")
        # Context 4: propertyValue opening
        apdu += bytes([0x4E])
        apdu += value_bytes
        # Context 4: propertyValue closing
        apdu += bytes([0x4F])
        # Context 1: listOfResults closing
        apdu += bytes([0x1F])
    return build_bvll(0x0A, apdu)

def parse_subscribe_cov(apdu):
    """Parse a SubscribeCOV-Request. Returns (invoke_id, process_id, object_type,
    object_instance, is_cancel) or None.
    """
    if len(apdu) < 4 or apdu[3] != 0x05:
        return None
    iid = apdu[2]
    pos = 4
    # Context 0: subscriberProcessIdentifier (0x09 1-byte or 0x0A 2-byte)
    tag = apdu[pos]
    if tag & 0xF8 != 0x08:
        return None
    lvt = tag & 0x07
    pos += 1
    pid = int.from_bytes(apdu[pos:pos+lvt], "big")
    pos += lvt
    # Context 1: monitoredObjectIdentifier (0x1C + 4 bytes)
    if pos + 5 > len(apdu) or apdu[pos] != 0x1C:
        return None
    raw = int.from_bytes(apdu[pos+1:pos+5], "big")
    ot = (raw >> 22) & 0x3FF
    oi = raw & 0x3F_FFFF
    pos += 5
    # Optional context 2 (issueConfirmedNotifications) and context 3 (lifetime)
    is_cancel = (pos >= len(apdu))
    return (iid, pid, ot, oi, is_cancel)

def parse_rpm_request(apdu):
    """Extract the list of (obj_type, instance, property_id) tuples from an RPM request.
    Returns [(ot, inst, pid), ...] or None if parsing fails.
    """
    if len(apdu) < 4 or apdu[3] != 0x0E:
        return None
    pos = 4
    specs = []
    while pos < len(apdu):
        if apdu[pos] != 0x0C:  # context 0 ObjectIdentifier
            break
        raw = int.from_bytes(apdu[pos+1:pos+5], "big")
        ot = (raw >> 22) & 0x3FF
        inst = raw & 0x3F_FFFF
        pos += 5
        if pos >= len(apdu) or apdu[pos] != 0x1E:  # context 1 opening
            break
        pos += 1
        # Parse inner property references until 0x1F
        while pos < len(apdu) and apdu[pos] != 0x1F:
            tag = apdu[pos]
            # context 0 PropertyIdentifier: 0x09 (1-byte) or 0x0A (2-byte)
            if tag == 0x09:
                pid = apdu[pos + 1]
                pos += 2
            elif tag == 0x0A:
                pid = int.from_bytes(apdu[pos+1:pos+3], "big")
                pos += 3
            else:
                # Unknown/optional tag — skip
                pos += 1 + (tag & 0x07)
                continue
            specs.append((ot, inst, pid))
            # Optional array index (context 1) — skip if present
            if pos < len(apdu) and (apdu[pos] & 0xF8) == 0x18:
                pos += 1 + (apdu[pos] & 0x07)
        if pos < len(apdu) and apdu[pos] == 0x1F:
            pos += 1  # consume closing tag
    return specs

def encode_unconfirmed_cov_notification(subscriber_process_id, device_instance, obj_type, obj_instance, time_remaining, value_bytes):
    """Encode an Unconfirmed-COV-Notification (service 0x02).

    Returns a complete BVLL-wrapped frame ready to send via UDP.
    """
    apdu = bytes([0x10, 0x02])  # Unconfirmed-Request, service 0x02
    # Context 0: subscriberProcessIdentifier
    apdu += encode_ctx_unsigned(0, subscriber_process_id)
    # Context 1: initiatingDeviceIdentifier (Device object)
    dev_id = ((8 & 0x3FF) << 22) | (device_instance & 0x3F_FFFF)
    apdu += bytes([0x1C]) + dev_id.to_bytes(4, "big")
    # Context 2: monitoredObjectIdentifier
    mon_id = ((obj_type & 0x3FF) << 22) | (obj_instance & 0x3F_FFFF)
    apdu += bytes([0x2C]) + mon_id.to_bytes(4, "big")
    # Context 3: timeRemaining
    apdu += encode_ctx_unsigned(3, time_remaining)
    # Context 4 opening: listOfValues
    apdu += bytes([0x4E])
    # PropertyValue: PresentValue (propertyIdentifier = 85)
    apdu += bytes([0x09, 0x55])  # context 0: propertyIdentifier = 85
    apdu += bytes([0x2E])        # context 2 opening: value
    apdu += value_bytes           # application-tagged value
    apdu += bytes([0x2F])        # context 2 closing
    # Context 4 closing
    apdu += bytes([0x4F])
    return build_bvll(0x0A, apdu)  # BVLL unicast


def main():
    bind_ip = sys.argv[1] if len(sys.argv) > 1 else "0.0.0.0"
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
    s.bind((bind_ip, 47808))
    print(f"hand-crafted BACnet sim on {bind_ip}:47808, device {DEVICE_INSTANCE}", flush=True)
    print(f"objects: {[(ot,inst,name) for ot,inst,name,_ in OBJECTS]}", flush=True)

    while True:
        data, addr = s.recvfrom(4096)
        print(f"RX {len(data)}B from {addr}: {data.hex()}", flush=True)

        if len(data) < 6 or data[0] != 0x81:
            continue

        # ── BVLL-level handlers (before APDU parsing) ──────────────────
        if data[1] == 0x05:  # BVLL Register-Foreign-Device
            ttl = int.from_bytes(data[4:6], "big") if len(data) >= 6 else 0
            print(f"  Register-Foreign-Device TTL={ttl}s from {addr}", flush=True)
            # BVLL-Result success
            result = bytes([0x81, 0x00, 0x00, 0x06, 0x00, 0x00])
            s.sendto(result, addr)
            print(f"  TX BVLL-Result(success) -> {addr}", flush=True)
            continue

        # bvll_type = data[1]  # 0x0A unicast or 0x0B broadcast
        # Skip 4-byte BVLL and 2-byte NPDU
        apdu = data[6:]
        if not apdu:
            continue

        pdu_type = apdu[0] & 0xF0
        if pdu_type == 0x10:  # Unconfirmed-Request
            if len(apdu) >= 2 and apdu[1] == 0x08:  # Who-Is
                reply = make_i_am()
                s.sendto(reply, addr)
                print(f"  TX I-Am -> {addr}: {reply.hex()}", flush=True)
        elif pdu_type == 0x00:  # Confirmed-Request
            header = parse_confirmed_request(apdu)
            if header is None:
                continue
            iid, service, _ = header
            if service == 0x0E:  # ReadPropertyMultiple
                specs = parse_rpm_request(apdu)
                if specs is None:
                    print("  (RPM parse failed)", flush=True)
                    continue
                print(f"  ReadPropertyMultiple invoke={iid} specs={specs}", flush=True)
                # Build results: one per spec, using the object's hardcoded value
                results = []
                for (ot, inst, pid) in specs:
                    if pid == 85:  # PresentValue
                        for (o_ot, o_inst, _, value_fn) in OBJECTS:
                            if o_ot == ot and o_inst == inst:
                                results.append((ot, inst, pid, value_fn()))
                                break
                if results:
                    reply = encode_rpm_ack(iid, results)
                    s.sendto(reply, addr)
                    print(f"  TX RPM-ACK -> {addr}: {reply.hex()[:80]}...", flush=True)
                continue
            if service == 0x05:  # SubscribeCOV
                parsed = parse_subscribe_cov(apdu)
                if parsed is None:
                    print("  (SubscribeCOV parse failed)", flush=True)
                    continue
                s_iid, s_pid, s_ot, s_oi, is_cancel = parsed
                action = "CANCEL" if is_cancel else "SUBSCRIBE"
                print(f"  SubscribeCOV {action} invoke={s_iid} proc={s_pid} obj={s_ot}:{s_oi}", flush=True)
                # Simple-ACK: [0x81, 0x0A, 0x00, 0x09, 0x01, 0x00, 0x20, iid, 0x05]
                ack = bytes([0x81, 0x0A, 0x00, 0x09, 0x01, 0x00, 0x20, s_iid, 0x05])
                s.sendto(ack, addr)
                print(f"  TX Simple-ACK -> {addr}: {ack.hex()}", flush=True)
                # After a successful SUBSCRIBE (not CANCEL), send an initial
                # Unconfirmed-COV-Notification with the object's current value.
                if not is_cancel:
                    for (o_ot, o_inst, _, value_fn) in OBJECTS:
                        if o_ot == s_ot and o_inst == s_oi:
                            cov = encode_unconfirmed_cov_notification(
                                s_pid, DEVICE_INSTANCE, s_ot, s_oi,
                                time_remaining=300,
                                value_bytes=value_fn(),
                            )
                            s.sendto(cov, addr)
                            print(f"  TX COV-Notification proc={s_pid} obj={s_ot}:{s_oi} -> {addr}: {cov.hex()[:80]}...", flush=True)
                            break
                continue
            if service == 0x0F:  # WriteProperty
                # Simple-ACK: [0x81, 0x0A, 0x00, 0x09, 0x01, 0x00, 0x20, iid, 0x0F]
                ack = bytes([0x81, 0x0A, 0x00, 0x09, 0x01, 0x00, 0x20, iid, 0x0F])
                s.sendto(ack, addr)
                print(f"  WriteProperty invoke={iid} -> TX Simple-ACK {ack.hex()}", flush=True)
                continue
            parsed = parse_read_property(apdu)
            if parsed:
                iid, ot, inst, pid, aidx = parsed
                print(f"  ReadProperty invoke={iid} obj={ot}:{inst} prop={pid} idx={aidx}", flush=True)
                reply = None
                if pid == 76:  # ObjectList
                    reply = make_object_list_ack(iid, inst)
                elif pid == 77:  # ObjectName
                    reply = make_object_name_ack(iid, ot, inst)
                elif pid == 85:  # PresentValue
                    reply = make_present_value_ack(iid, ot, inst)
                if reply:
                    s.sendto(reply, addr)
                    print(f"  TX ack -> {addr}: {reply.hex()[:80]}...", flush=True)
                else:
                    print("  (no handler for property)", flush=True)

if __name__ == "__main__":
    main()
