import fcntl, socket, struct, time, os, sys
name = sys.argv[1] if len(sys.argv) > 1 else "tun0"
for _ in range(600):
    if os.path.exists(f"/sys/class/net/{name}"): break
    time.sleep(0.2)
else:
    print("iface never appeared"); sys.exit(1)
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
def ifreq(name, payload): return struct.pack("16s", name.encode()) + payload
SIOCSIFADDR, SIOCSIFNETMASK, SIOCSIFFLAGS, SIOCGIFFLAGS, SIOCSIFMTU = 0x8916, 0x891c, 0x8914, 0x8913, 0x8922
def sockaddr(ip): return struct.pack("H2s4s8x", socket.AF_INET, b"\0\0", socket.inet_aton(ip))
fcntl.ioctl(s, SIOCSIFADDR, ifreq(name, sockaddr("10.0.0.1")))
fcntl.ioctl(s, SIOCSIFNETMASK, ifreq(name, sockaddr("255.255.255.0")))
fl = struct.unpack("16sH", fcntl.ioctl(s, SIOCGIFFLAGS, ifreq(name, b"\0"*24))[:18])[1]
fcntl.ioctl(s, SIOCSIFFLAGS, ifreq(name, struct.pack("H", fl | 1)))  # IFF_UP
print("configured", name)
