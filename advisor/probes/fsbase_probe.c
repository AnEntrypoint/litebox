// Probe: when does Windows clear a user-written FS base? (wrfsbase needs CR4.FSGSBASE)
#include <windows.h>
#include <stdio.h>
#include <stdint.h>
static inline void wrfs(uint64_t v) { __asm__ volatile("wrfsbase %0" :: "r"(v)); }
static inline uint64_t rdfs(void) { uint64_t v; __asm__ volatile("rdfsbase %0" : "=r"(v)); return v; }
static LONG CALLBACK veh(EXCEPTION_POINTERS *ep) {
    if (ep->ExceptionRecord->ExceptionCode == EXCEPTION_ILLEGAL_INSTRUCTION) { printf("wrfsbase/rdfsbase #UD: FSGSBASE not enabled for user mode\n"); ExitProcess(3); }
    if (ep->ExceptionRecord->ExceptionCode == EXCEPTION_ACCESS_VIOLATION) { ep->ContextRecord->Rip += 7; return EXCEPTION_CONTINUE_EXECUTION; } // skip 'mov eax,[abs32]' style 7-byte insn we plant
    return EXCEPTION_CONTINUE_SEARCH;
}
static void report(const char *what, uint64_t expect) { uint64_t v = rdfs(); printf("%-40s fsbase=%#llx %s\n", what, (unsigned long long)v, v == expect ? "kept" : "CLEARED"); }
int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    AddVectoredExceptionHandler(1, veh);
    uint64_t want = 0x12345678000ULL;
    int lost = 0;
    wrfs(want); report("immediately after wrfsbase", want);
    wrfs(want); for (volatile int i = 0; i < 100000000; i++); report("after 100M-iteration user spin", want);
    wrfs(want); Sleep(0); report("after Sleep(0) (syscall, maybe switch)", want);
    wrfs(want); Sleep(20); report("after Sleep(20) (context switch)", want);
    wrfs(want); { LARGE_INTEGER li; QueryPerformanceCounter(&li); } report("after QueryPerformanceCounter (no syscall)", want);
    wrfs(want); { HANDLE h = GetCurrentProcess(); DWORD c; GetExitCodeProcess(h, &c); } report("after GetExitCodeProcess (syscall)", want);
    wrfs(want); { volatile int *p = (volatile int *)0x10; __asm__ volatile("mov 0x10, %%eax" ::: "eax"); } report("after VEH-handled access violation", want);
    // statistics: how often is it cleared across 1000 fast syscalls?
    for (int i = 0; i < 1000; i++) { wrfs(want); DWORD c; GetExitCodeProcess(GetCurrentProcess(), &c); if (rdfs() != want) lost++; }
    printf("cleared after %d of 1000 GetExitCodeProcess syscalls\n", lost);
    lost = 0;
    for (int i = 0; i < 1000; i++) { wrfs(want); SwitchToThread(); if (rdfs() != want) lost++; }
    printf("cleared after %d of 1000 SwitchToThread calls\n", lost);
    return 0;
}
