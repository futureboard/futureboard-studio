#!/usr/bin/env python3
"""Convert an interleaved f32le reference decode into the compact .ref format the mkff-ac3
tests read: every `step`-th sample frame, as 24-bit signed integers (value * 2^23).

    make_ref.py in.f32 channels step out.ref [--take a,b,c] [--from other.f32:channels:i,j]

Header: b"MKFFREF1", u32 channels, u32 step, u32 frames (little endian), then the samples.
--take keeps the listed channels of the input (in that order); --from appends channels taken
from a second decode (same length) after them.
"""
import struct
import sys


def load(path, nch):
    data = open(path, 'rb').read()
    n = len(data) // 4 // nch
    vals = struct.unpack('<%df' % (n * nch), data[:n * nch * 4])
    return [[vals[i * nch + c] for i in range(n)] for c in range(nch)]


def main():
    path, nch, step, out = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
    chans = load(path, nch)
    args = sys.argv[5:]
    while args:
        opt, val = args[0], args[1]
        args = args[2:]
        if opt == '--take':
            chans = [chans[int(c)] for c in val.split(',')]
        elif opt == '--from':
            other, onch, idx = val.rsplit(':', 2)
            oc = load(other, int(onch))
            chans += [oc[int(c)] for c in idx.split(',')]
    n = min(len(c) for c in chans)
    pos = list(range(0, n, step))
    body = bytearray()
    for i in pos:
        for c in chans:
            v = max(-(1 << 23), min((1 << 23) - 1, round(c[i] * (1 << 23))))
            body += struct.pack('<i', v)[:3]
    with open(out, 'wb') as f:
        f.write(b'MKFFREF1' + struct.pack('<III', len(chans), step, len(pos)) + bytes(body))


main()
