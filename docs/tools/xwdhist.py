# xwdhist.py <out-file> : decode each "XWD<n>\n" + raw xwd + "\nEND-XWD" block printed by wt.sh.
import sys, struct, collections
d = open(sys.argv[1], 'rb').read()
i = 0; k = 0
while True:
    j = d.find(b'XWD', i)
    if j < 0: break
    nl = d.find(b'\n', j)
    try: n = int(d[j+3:nl])
    except ValueError: i = j + 3; continue
    raw = d[nl+1:nl+1+n]
    i = nl + 1 + n
    if len(raw) < 100: continue
    h = struct.unpack('>25I', raw[:100])
    hdr, w, hgt, bpp, bpl, ncol = h[0], h[4], h[5], h[11], h[12], h[19]
    off = hdr + 12 * ncol
    px = raw[off:]
    bp = bpp // 8; cnt = collections.Counter(px[(y*bpl)+x*bp:(y*bpl)+x*bp+bp] for y in range(hgt) for x in range(w))
    black = cnt.get(b'\x00\x00\x00\x00', 0)
    k += 1
    print(f"capture {k}: {w}x{hgt} bpp={bpp} bytes={n} distinct={len(cnt)} top={[(c.hex(), v) for c, v in cnt.most_common(3)]}")
print("captures:", k)
