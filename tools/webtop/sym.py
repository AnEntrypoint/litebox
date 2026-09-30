import re,subprocess,sys
t=open(sys.argv[1],errors='replace').read()
base=int(re.search(r'litebox-exe-base: ([0-9a-f]+)',t).group(1),16)
R='/home/user/litebox/target/release/litebox_runner_linux_userland'
seen=set()
for m in re.finditer(r"thread '[^']*' \(\d+\) panicked at ([^\n]*)\n([^\n]*)\n(?:.*?\n)*?litebox-panic-frames:([^\n]*)",t):
    key=m.group(1)
    if key in seen: continue
    seen.add(key)
    print("PANIC",m.group(1),"|",m.group(2)[:80])
    addrs=[int(a,16) for a in m.group(3).split()]
    out=subprocess.run(['addr2line','-f','-C','-i','-e',R]+[hex(a-base-1) for a in addrs],capture_output=True,text=True).stdout.split('\n')
    for i in range(0,len(out)-1,2):
        fn=out[i]; loc=out[i+1]
        if '/rustc/' in loc or '.cargo' in loc or fn.startswith('std::') or fn.startswith('core::'): continue
        print("   ",fn[:100],'@',loc.split('/litebox/')[-1][:60])
