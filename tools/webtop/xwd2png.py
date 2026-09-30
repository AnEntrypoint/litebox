import struct,sys,zlib
d=open(sys.argv[1],'rb').read()
h=struct.unpack('>25I',d[:100])
hs,w,ht,bpl,bpp,ncol=h[0],h[4],h[5],h[12],h[11],h[19]
off=hs+ncol*12
px=d[off:off+bpl*ht]
raw=bytearray()
for y in range(ht):
    row=px[y*bpl:y*bpl+w*4]
    out=bytearray(w*3)
    out[0::3]=row[2::4]; out[1::3]=row[1::4]; out[2::3]=row[0::4]
    raw+=b'\0'+out
def ch(t,b): 
    c=struct.pack('>I',len(b))+t+b
    return c+struct.pack('>I',zlib.crc32(t+b))
png=b'\x89PNG\r\n\x1a\n'+ch(b'IHDR',struct.pack('>IIBBBBB',w,ht,8,2,0,0,0))+ch(b'IDAT',zlib.compress(bytes(raw),6))+ch(b'IEND',b'')
open(sys.argv[2],'wb').write(png); print(w,ht,bpp)
