"""Minimal Parquet reader: thrift-compact footer, v1/v2 data pages, PLAIN /
RLE_DICTIONARY for the columns we need, SNAPPY or uncompressed."""
import struct, sys, json


class Thrift:
    def __init__(self, b, pos=0):
        self.b, self.p = b, pos

    def byte(self):
        v = self.b[self.p]; self.p += 1; return v

    def varint(self):
        r = s = 0
        while True:
            x = self.byte(); r |= (x & 0x7F) << s; s += 7
            if not x & 0x80: return r

    def zz(self):
        v = self.varint(); return (v >> 1) ^ -(v & 1)

    def value(self, t):
        if t == 1: return True
        if t == 2: return False
        if t == 3: return struct.unpack('b', bytes([self.byte()]))[0]
        if t in (4, 5, 6): return self.zz()
        if t == 7: v = struct.unpack('<d', self.b[self.p:self.p + 8])[0]; self.p += 8; return v
        if t == 8:
            n = self.varint(); v = self.b[self.p:self.p + n]; self.p += n; return v
        if t in (9, 10):
            h = self.byte(); n = h >> 4; et = h & 0x0F
            if n == 15: n = self.varint()
            return [self.value(et) for _ in range(n)]
        if t == 11:
            n = self.varint()
            if n == 0: return {}
            kv = self.byte(); return {self.value(kv >> 4): self.value(kv & 0x0F) for _ in range(n)}
        if t == 12: return self.struct()
        raise ValueError(t)

    def struct(self):
        out = {}; last = 0
        while True:
            h = self.byte()
            if h == 0: return out
            d = h >> 4; t = h & 0x0F
            fid = last + d if d else self.zz()
            last = fid
            out[fid] = self.value(t)


def snappy(src):
    i = 0; n = 0; s = 0
    while True:
        x = src[i]; i += 1; n |= (x & 0x7F) << s; s += 7
        if not x & 0x80: break
    out = bytearray()
    while i < len(src):
        tag = src[i]; i += 1; kind = tag & 3
        if kind == 0:
            ln = tag >> 2
            if ln >= 60:
                nb = ln - 59; ln = int.from_bytes(src[i:i + nb], 'little'); i += nb
            ln += 1; out += src[i:i + ln]; i += ln
        else:
            if kind == 1:
                ln = ((tag >> 2) & 7) + 4; off = ((tag >> 5) << 8) | src[i]; i += 1
            elif kind == 2:
                ln = (tag >> 2) + 1; off = int.from_bytes(src[i:i + 2], 'little'); i += 2
            else:
                ln = (tag >> 2) + 1; off = int.from_bytes(src[i:i + 4], 'little'); i += 4
            for _ in range(ln): out.append(out[-off])
    assert len(out) == n
    return bytes(out)


def rle_bitpacked(buf, bit_width, count):
    """RLE/bit-packed hybrid, `count` values."""
    t = Thrift(buf); out = []
    bw_bytes = (bit_width + 7) // 8
    while len(out) < count and t.p < len(buf):
        h = t.varint()
        if h & 1:
            groups = h >> 1; nbytes = groups * bit_width
            bits = int.from_bytes(buf[t.p:t.p + nbytes], 'little'); t.p += nbytes
            for k in range(groups * 8):
                out.append((bits >> (k * bit_width)) & ((1 << bit_width) - 1))
        else:
            run = h >> 1; v = int.from_bytes(buf[t.p:t.p + bw_bytes], 'little'); t.p += bw_bytes
            out += [v] * run
    return out[:count]


def read(path):
    b = open(path, 'rb').read()
    flen = struct.unpack('<I', b[-8:-4])[0]
    meta = Thrift(b, len(b) - 8 - flen).struct()
    schema = [(e.get(4, b'').decode(), e.get(1)) for e in meta[2]]
    cols = {}
    for rg in meta[4]:
        for cc in rg[1]:
            md = cc[3]
            name = '.'.join(x.decode() for x in md[3])
            codec = md[4]; ptype = md[1]
            start = md.get(11, md[9]); nvals = md[5]
            pos = start; values = []; dictionary = None
            end = start + md[7]
            while pos < end:
                th = Thrift(b, pos); ph = th.struct(); pos = th.p
                comp = ph[3]; body = b[pos:pos + comp]; pos += comp
                typ = ph[1]
                if typ == 2:  # dictionary page
                    raw = snappy(body) if codec == 1 else body
                    dictionary = plain(raw, ptype, ph[7][1])
                    continue
                if typ == 0:  # data page v1
                    dph = ph[5]; nv = dph[1]; enc = dph[2]
                    raw = snappy(body) if codec == 1 else body
                    # definition levels (optional column): length-prefixed RLE
                    p = 0
                    if dph.get(3, 0) == 3 or True:
                        ln = struct.unpack('<I', raw[:4])[0]; p = 4 + ln
                    data = raw[p:]
                elif typ == 3:  # data page v2
                    dph = ph[8]; nv = dph[1]; enc = dph[4]
                    dl = dph[5]; rl = dph[6]
                    lv = body[:dl + rl]; rest = body[dl + rl:]
                    compressed = dph.get(7, True)
                    data = snappy(rest) if (codec == 1 and compressed) else rest
                else:
                    continue
                if enc in (8, 2):  # RLE_DICTIONARY / PLAIN_DICTIONARY
                    bw = data[0]
                    idx = rle_bitpacked(data[1:], bw, nv)
                    values += [dictionary[i] for i in idx]
                else:
                    values += plain(data, ptype, nv)
            cols.setdefault(name, []).extend(values)
    return schema, cols


def plain(buf, ptype, n):
    if ptype == 6:  # BYTE_ARRAY
        out = []; p = 0
        while len(out) < n and p < len(buf):
            ln = struct.unpack('<I', buf[p:p + 4])[0]; out.append(buf[p + 4:p + 4 + ln]); p += 4 + ln
        return out
    if ptype == 1: return list(struct.unpack(f'<{n}i', buf[:4 * n]))
    if ptype == 2: return list(struct.unpack(f'<{n}q', buf[:8 * n]))
    raise ValueError(ptype)


if __name__ == '__main__':
    schema, cols = read(sys.argv[1])
    print(schema)
    for k, v in cols.items():
        print(k, len(v), [x if not isinstance(x, bytes) else (x[:40] if len(x) < 200 else len(x)) for x in v[:4]])
