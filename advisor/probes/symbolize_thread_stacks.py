#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT license.
import re,subprocess,sys
exe=r'C:\dev\litebox-main\target\release\litebox_runner_linux_on_windows_userland.exe'
for f in sys.argv[1:]:
    t=open(f,encoding='utf-8',errors='ignore').read()
    parts=re.split(r'\n\s*[.#]?\s*\d+\s+Id:',t)[1:]
    print('=====',f,len(parts),'threads')
    for p in parts:
        hdr=p.split('\n')[0]
        rv=re.findall(r'litebox_runner_linux_on_windows_userland\+0x([0-9a-f]+)',p)
        top=re.findall(r'(?:ntdll|KERNELBASE|ws2_32|mswsock|KERNEL32)!(\w+)',p)[:2]
        out=subprocess.run(['llvm-symbolizer','--obj='+exe,'--relative-address','--no-inlines','--demangle']+['0x'+x for x in rv[:7]],capture_output=True,text=True).stdout if rv else ''
        names=[l.split('::')[-1][:60] if 'litebox' not in l else l[-70:] for l in out.split('\n') if l and not l.startswith(' ') and ':' not in l[:3] and 'rustc' not in l]
        print('--',hdr[hdr.find('"'):][:25],top,'|',' > '.join(n.strip() for n in names[:6]))
