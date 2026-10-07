import struct, sys, collections

def analyze(path):
    with open(path, 'rb') as f:
        data = f.read()
    off = struct.unpack_from('<I', data, 10)[0]
    w = struct.unpack_from('<i', data, 18)[0]
    h = struct.unpack_from('<i', data, 22)[0]
    top_down = h < 0
    h = abs(h)
    px = data[off:off + w * h * 4]
    colors = collections.Counter()
    rows_nonblack = collections.Counter()
    minx = miny = 10**9
    maxx = maxy = -1
    stride = w * 4
    for y in range(h):
        row_i = y if top_down else (h - 1 - y)
        row = px[row_i * stride:(row_i + 1) * stride]
        # fast path: skip rows that are entirely one of the two black encodings
        if row.count(b'\x00\x00\x00\xff') * 4 == len(row) or row.count(b'\x00\x00\x00\x00') * 4 == len(row):
            colors[(0, 0, 0, row[3])] += w
            continue
        for x in range(w):
            b, g, r, a = row[x * 4:x * 4 + 4]
            colors[(r, g, b, a)] += 1
            if not ((r, g, b) == (0, 0, 0) and a in (0, 255)):
                rows_nonblack[y] += 1
                minx = min(minx, x); maxx = max(maxx, x)
                miny = min(miny, y); maxy = max(maxy, y)
    total_nb = sum(rows_nonblack.values())
    print(f"{path}: {w}x{h} top_down={top_down} non_black={total_nb} bbox=x[{minx},{maxx}] y[{miny},{maxy}]")
    print("  top colors (r,g,b,a):count:", colors.most_common(8))
    ys = sorted(rows_nonblack)
    if ys:
        # summarize row bands
        bands = []
        start = prev = ys[0]
        for y in ys[1:]:
            if y != prev + 1:
                bands.append((start, prev, sum(rows_nonblack[k] for k in range(start, prev + 1))))
                start = y
            prev = y
        bands.append((start, prev, sum(rows_nonblack[k] for k in range(start, prev + 1))))
        print("  non-black row bands (y0,y1,count):", bands[:12])

for p in sys.argv[1:]:
    analyze(p)
