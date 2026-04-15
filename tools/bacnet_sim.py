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
