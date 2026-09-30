import os,subprocess,re,struct,sys
root=int(subprocess.check_output(['pgrep','-x','litebox_runner_']).split()[0])
addr=None
for t in os.listdir(f'/proc/{root}/task'):
    try: sc=open(f'/proc/{root}/task/{t}/syscall').read().split()
    except: continue
    if sc[0]=='202': print('thread',t,'futex',sc[1])
    if sc[0]=='202' and t!=str(root): addr=int(sc[1],16)
print('addr',hex(addr) if addr else None)
out=subprocess.run(['gdb','-p',str(root),'-batch','-ex',f'x/8wx {hex(addr-4)}'],capture_output=True,text=True).stdout
print(out[-300:])
