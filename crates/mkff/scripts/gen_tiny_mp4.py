#!/usr/bin/env python3
"""Generate checked-in progressive MP4 fixtures from Annex-B testdata.

No FFmpeg required. Writes ISOBMFF moov+mdat with avc1/avcC or hvc1/hvcC.
"""

from __future__ import annotations

import struct
import sys
from pathlib import Path


def be16(n: int) -> bytes:
    return struct.pack(">H", n & 0xFFFF)


def be32(n: int) -> bytes:
    return struct.pack(">I", n & 0xFFFFFFFF)


def box(typ: bytes, payload: bytes) -> bytes:
    assert len(typ) == 4
    return be32(8 + len(payload)) + typ + payload


def full_box(typ: bytes, version: int, flags: int, payload: bytes) -> bytes:
    hdr = bytes([version, (flags >> 16) & 0xFF, (flags >> 8) & 0xFF, flags & 0xFF])
    return box(typ, hdr + payload)


def split_annexb(data: bytes) -> list[bytes]:
    """Return raw NAL payloads (without start codes)."""
    starts: list[tuple[int, int]] = []  # (sc_pos, nal_pos)
    i = 0
    while i + 3 <= len(data):
        if data[i : i + 4] == b"\x00\x00\x00\x01":
            starts.append((i, i + 4))
            i += 4
        elif data[i : i + 3] == b"\x00\x00\x01":
            starts.append((i, i + 3))
            i += 3
        else:
            i += 1
    nals: list[bytes] = []
    for idx, (_sc, nal_pos) in enumerate(starts):
        end = starts[idx + 1][0] if idx + 1 < len(starts) else len(data)
        nal = data[nal_pos:end]
        if nal:
            nals.append(nal)
    return nals


def h264_type(nal: bytes) -> int:
    return nal[0] & 0x1F


def hevc_type(nal: bytes) -> int:
    return (nal[0] >> 1) & 0x3F


def length_prefixed(nals: list[bytes]) -> bytes:
    out = bytearray()
    for nal in nals:
        out += be32(len(nal))
        out += nal
    return bytes(out)


def make_avcc(sps_list: list[bytes], pps_list: list[bytes]) -> bytes:
    sps = sps_list[0]
    body = bytearray()
    body += bytes([1, sps[1], sps[2], sps[3], 0xFF])  # lengthSizeMinusOne = 3
    body.append(0xE0 | (len(sps_list) & 0x1F))
    for s in sps_list:
        body += be16(len(s)) + s
    body.append(len(pps_list) & 0xFF)
    for p in pps_list:
        body += be16(len(p)) + p
    return box(b"avcC", bytes(body))


def make_hvcc(vps_list: list[bytes], sps_list: list[bytes], pps_list: list[bytes]) -> bytes:
    body = bytearray()
    body.append(1)  # configurationVersion
    body.append(1)  # general_profile_space/tier/idc
    body += be32(0x60000000)
    body += bytes(6)
    body.append(120)
    body += be16(0xF000)
    body.append(0xFC)
    body.append(0xFD)
    body.append(0xF8)
    body.append(0xF8)
    body += be16(0)
    body.append(0x03)  # lengthSizeMinusOne = 3
    arrays = [(32, vps_list), (33, sps_list), (34, pps_list)]
    body.append(len(arrays))
    for nal_type, nals in arrays:
        body.append(0x80 | (nal_type & 0x3F))
        body += be16(len(nals))
        for nal in nals:
            body += be16(len(nal)) + nal
    return box(b"hvcC", bytes(body))


def visual_sample_entry(coding: bytes, width: int, height: int, cfg: bytes) -> bytes:
    payload = bytearray()
    payload += bytes(6)
    payload += be16(1)
    payload += be16(0)
    payload += be16(0)
    payload += be32(0) * 3
    payload += be16(width)
    payload += be16(height)
    payload += be32(0x00480000)
    payload += be32(0x00480000)
    payload += be32(0)
    payload += be16(1)
    payload += bytes(32)
    payload += be16(0x0018)
    payload += be16(0xFFFF)
    payload += cfg
    return box(coding, bytes(payload))


def build_mp4(
    *,
    coding: bytes,
    width: int,
    height: int,
    cfg: bytes,
    sample_bytes: bytes,
    timescale: int = 30,
) -> bytes:
    ftyp = box(b"ftyp", b"isom" + be32(512) + b"isom" + b"iso2" + coding + b"mp41")

    stsd = full_box(b"stsd", 0, 0, be32(1) + visual_sample_entry(coding, width, height, cfg))
    stts = full_box(b"stts", 0, 0, be32(1) + be32(1) + be32(1))
    stsc = full_box(b"stsc", 0, 0, be32(1) + be32(1) + be32(1) + be32(1))
    stsz = full_box(b"stsz", 0, 0, be32(0) + be32(1) + be32(len(sample_bytes)))
    stss = full_box(b"stss", 0, 0, be32(1) + be32(1))
    stco = full_box(b"stco", 0, 0, be32(1) + be32(0))  # offset patched below

    stbl = box(b"stbl", stsd + stts + stsc + stsz + stco + stss)
    url = full_box(b"url ", 0, 1, b"")
    dref = full_box(b"dref", 0, 0, be32(1) + url)
    dinf = box(b"dinf", dref)
    vmhd = full_box(b"vmhd", 0, 1, be16(0) + be16(0) + be16(0) + be16(0))
    minf = box(b"minf", vmhd + dinf + stbl)
    hdlr = full_box(b"hdlr", 0, 0, be32(0) + b"vide" + be32(0) * 3 + b"VideoHandler\x00")
    mdhd = full_box(
        b"mdhd",
        0,
        0,
        be32(0) + be32(0) + be32(timescale) + be32(1) + be16(0x55C4) + be16(0),
    )
    mdia = box(b"mdia", mdhd + hdlr + minf)

    unity = (
        be32(0x00010000)
        + be32(0) * 3
        + be32(0)
        + be32(0x00010000)
        + be32(0) * 3
        + be32(0)
        + be32(0x40000000)
    )
    tkhd = full_box(
        b"tkhd",
        0,
        3,
        be32(0)
        + be32(0)
        + be32(1)
        + be32(0)
        + be32(1)
        + be32(0) * 2
        + be16(0)
        + be16(0)
        + be16(0)
        + be16(0)
        + unity
        + be32(width << 16)
        + be32(height << 16),
    )
    trak = box(b"trak", tkhd + mdia)

    mvhd = full_box(
        b"mvhd",
        0,
        0,
        be32(0)
        + be32(0)
        + be32(timescale)
        + be32(1)
        + be32(0x00010000)
        + be16(0x0100)
        + be16(0)
        + be32(0) * 2
        + unity
        + be32(0) * 6
        + be32(2),
    )
    moov = bytearray(box(b"moov", mvhd + trak))

    mdat_offset = len(ftyp) + len(moov) + 8
    idx = moov.rfind(b"stco")
    if idx < 0:
        raise RuntimeError("stco missing")
    # type@idx, ver/flags@idx+4, count@idx+8, offset@idx+12
    moov[idx + 12 : idx + 16] = be32(mdat_offset)

    mdat = box(b"mdat", sample_bytes)
    return bytes(ftyp) + bytes(moov) + mdat


def convert_h264(path: Path, out: Path, width: int, height: int) -> None:
    nals = split_annexb(path.read_bytes())
    sps = [n for n in nals if h264_type(n) == 7]
    pps = [n for n in nals if h264_type(n) == 8]
    # Sample payload: everything except SPS/PPS (AUD/SEI/VCL, …).
    sample_nals = [n for n in nals if h264_type(n) not in (7, 8)]
    if not sps or not pps or not any(h264_type(n) in (1, 5) for n in sample_nals):
        raise SystemExit(f"{path}: need SPS/PPS/VCL, got {[h264_type(n) for n in nals]}")
    out.write_bytes(
        build_mp4(
            coding=b"avc1",
            width=width,
            height=height,
            cfg=make_avcc(sps, pps),
            sample_bytes=length_prefixed(sample_nals),
        )
    )
    print(f"wrote {out} ({out.stat().st_size} bytes, {len(sample_nals)} sample NAL(s))")


def convert_hevc(path: Path, out: Path, width: int, height: int) -> None:
    nals = split_annexb(path.read_bytes())
    vps = [n for n in nals if hevc_type(n) == 32]
    sps = [n for n in nals if hevc_type(n) == 33]
    pps = [n for n in nals if hevc_type(n) == 34]
    # Sample payload: SEI + VCL etc.; VPS/SPS/PPS live in hvcC.
    sample_nals = [n for n in nals if hevc_type(n) not in (32, 33, 34)]
    if not vps or not sps or not pps or not any(0 <= hevc_type(n) <= 31 for n in sample_nals):
        raise SystemExit(f"{path}: need VPS/SPS/PPS/VCL, got {[hevc_type(n) for n in nals]}")
    out.write_bytes(
        build_mp4(
            coding=b"hvc1",
            width=width,
            height=height,
            cfg=make_hvcc(vps, sps, pps),
            sample_bytes=length_prefixed(sample_nals),
        )
    )
    print(f"wrote {out} ({out.stat().st_size} bytes, {len(sample_nals)} sample NAL(s))")

def main() -> int:
    root = Path(__file__).resolve().parents[1]
    td = root / "testdata"
    convert_h264(td / "tiny_baseline_64x64.h264", td / "tiny_baseline_64x64.mp4", 64, 64)
    convert_hevc(td / "tiny_main_256x144.hevc", td / "tiny_main_256x144.mp4", 256, 144)
    return 0


if __name__ == "__main__":
    sys.exit(main())
