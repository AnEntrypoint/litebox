// Probe (advisory Appendix D, risk 1): can the runner hand a scanout section HANDLE to a
// separately spawned presenter process via DuplicateHandle, with no admin rights, and have the
// presenter map the very same pixels? Parent creates a section, spawns the child, duplicates the
// handle into it, sends the handle value over a pipe; child maps it and verifies the contents,
// then writes through a read-write duplicate to prove liveness both ways.
#include <windows.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>

#define PAT 0xA5A5BEEFu
#define PAT2 0x5A5AF00Du

static int child_main(void) {
    char line[64];
    if (!fgets(line, sizeof line, stdin)) { printf("CHILD: no handle received\n"); return 2; }
    HANDLE h = (HANDLE)(ULONG_PTR)strtoull(line, NULL, 16);
    unsigned *p = (unsigned *)MapViewOfFile(h, FILE_MAP_ALL_ACCESS, 0, 0, 4096);
    if (!p) { printf("CHILD: MapViewOfFile failed err=%lu\n", GetLastError()); return 3; }
    printf("CHILD: mapped inherited section, word0=0x%08X %s\n", p[0],
           p[0] == PAT ? "(correct - zero-copy scanout works)" : "(WRONG)");
    if (p[0] != PAT) return 4;
    p[1] = PAT2;                       // prove the mapping is live and writable back to the parent
    printf("CHILD: wrote 0x%08X into word1\n", PAT2);
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc >= 2 && strcmp(argv[1], "--child") == 0) return child_main();

    HANDLE sec = CreateFileMappingW(INVALID_HANDLE_VALUE, NULL, PAGE_READWRITE, 0, 4096, NULL);
    if (!sec) { printf("CreateFileMapping failed %lu\n", GetLastError()); return 2; }
    unsigned *v = (unsigned *)MapViewOfFile(sec, FILE_MAP_ALL_ACCESS, 0, 0, 4096);
    v[0] = PAT; v[1] = 0;

    HANDLE rd = NULL, wr = NULL;
    SECURITY_ATTRIBUTES sa = { sizeof(sa), NULL, TRUE };
    if (!CreatePipe(&rd, &wr, &sa, 0)) { printf("CreatePipe failed\n"); return 2; }
    SetHandleInformation(wr, HANDLE_FLAG_INHERIT, 0);   // keep our write end private

    STARTUPINFOW si; PROCESS_INFORMATION pi;
    memset(&si, 0, sizeof si); si.cb = sizeof si;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = rd;
    si.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
    si.hStdError = GetStdHandle(STD_ERROR_HANDLE);

    wchar_t exe[MAX_PATH]; GetModuleFileNameW(NULL, exe, MAX_PATH);
    wchar_t cmd[MAX_PATH + 32]; swprintf(cmd, MAX_PATH + 32, L"\"%s\" --child", exe);

    if (!CreateProcessW(NULL, cmd, NULL, NULL, TRUE, 0, NULL, NULL, &si, &pi)) {
        printf("CreateProcess failed %lu\n", GetLastError()); return 2;
    }
    // We already hold a full-access handle to the process we created: no admin, no SeDebugPrivilege.
    HANDLE dup = NULL;
    BOOL ok = DuplicateHandle(GetCurrentProcess(), sec, pi.hProcess, &dup,
                              0, FALSE, DUPLICATE_SAME_ACCESS);
    printf("PARENT: DuplicateHandle into presenter -> ok=%d err=%lu value=0x%llx\n",
           ok, ok ? 0UL : GetLastError(), (unsigned long long)(ULONG_PTR)dup);
    if (!ok) { TerminateProcess(pi.hProcess, 1); return 5; }

    char line[64];
    int n = snprintf(line, sizeof line, "%llx\n", (unsigned long long)(ULONG_PTR)dup);
    DWORD w = 0; WriteFile(wr, line, (DWORD)n, &w, NULL);

    DWORD waited = WaitForSingleObject(pi.hProcess, 10000);
    DWORD code = 1; GetExitCodeProcess(pi.hProcess, &code);
    if (waited != WAIT_OBJECT_0) { printf("PARENT: child timed out\n"); TerminateProcess(pi.hProcess, 1); return 6; }
    printf("PARENT: child exit=%lu; word1 now 0x%08X %s\n", code, v[1],
           v[1] == PAT2 ? "(child's write visible to runner - live shared scanout)" : "(not visible)");
    return (code == 0 && v[1] == PAT2) ? 0 : 7;
}
