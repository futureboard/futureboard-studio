"""Concatenate (slices of) the audio packets of Ogg Opus files into one Ogg Opus stream.

Usage: python oggcat.py OUT.opus IN.opus[START:END] [IN2.opus[START:END] ...]

The identification and comment headers come from the first input; granule positions are
recomputed so that only the first input's pre-skip is trimmed by players (no end trimming).
Development tool for the mkff-opus fixtures; not used by the tests.
"""
import re
import struct
import sys


def _crc_table():
    t = []
    for i in range(256):
        r = i << 24
        for _ in range(8):
            r = ((r << 1) ^ 0x04C11DB7) if r & 0x80000000 else (r << 1)
        t.append(r & 0xFFFFFFFF)
    return t


_T = _crc_table()


def crc(data):
    c = 0
    for b in data:
        c = ((c << 8) ^ _T[((c >> 24) ^ b) & 0xFF]) & 0xFFFFFFFF
    return c


def packets(path):
    d = open(path, 'rb').read()
    pos, cur, out = 0, b'', []
    while pos + 27 <= len(d):
        assert d[pos:pos + 4] == b'OggS'
        n = d[pos + 26]
        lac = d[pos + 27:pos + 27 + n]
        body = pos + 27 + n
        for l in lac:
            cur += d[body:body + l]
            body += l
            if l < 255:
                out.append(cur)
                cur = b''
        pos = body
    return out


def samples(p):
    cfg = p[0] >> 3
    if cfg < 12:
        fs = [480, 960, 1920, 2880][cfg & 3]
    elif cfg < 16:
        fs = 480 if cfg & 1 == 0 else 960
    else:
        fs = 120 << (cfg & 3)
    c = p[0] & 3
    return fs * (1 if c == 0 else 2 if c < 3 else p[1] & 0x3F)


def page(serial, seq, granule, flags, pkts):
    lac, body = b'', b''
    for p in pkts:
        n = len(p)
        while n >= 255:
            lac += b'\xff'
            n -= 255
        lac += bytes([n])
        body += p
    pg = (b'OggS' + bytes([0, flags]) + struct.pack('<qII', granule, serial, seq) + b'\0\0\0\0'
          + bytes([len(lac)]) + lac + body)
    return pg[:22] + struct.pack('<I', crc(pg)) + pg[26:]


def main():
    out_path, specs = sys.argv[1], sys.argv[2:]
    head = tags = None
    audio = []
    for spec in specs:
        m = re.fullmatch(r'(.*?)(?:\[(\d*):(\d*)\])?', spec)
        ps = packets(m.group(1))
        if head is None:
            head, tags = ps[0], ps[1]
        lo = int(m.group(2)) if m.group(2) else 0
        hi = int(m.group(3)) if m.group(3) else None
        audio += ps[2:][lo:hi]
    serial, seq, g = 0x4D4B4646, 0, 0
    out = page(serial, seq, 0, 2, [head])
    seq += 1
    out += page(serial, seq, 0, 0, [tags])
    seq += 1
    for i, p in enumerate(audio):
        g += samples(p)
        out += page(serial, seq, g, 4 if i == len(audio) - 1 else 0, [p])
        seq += 1
    open(out_path, 'wb').write(out)


if __name__ == '__main__':
    main()
