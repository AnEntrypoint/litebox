// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Writes a fixed marker string into a file under /tmp. Paired with
// persist_read.c for an --export-writable-layer/--resume-from round-trip
// test (see test_export_and_resume_writable_layer in run.rs): the writable
// layer this program writes into gets exported to a tar archive by one
// runner invocation, then a second, independent runner invocation resumes
// from that archive and reads the file back via persist_read.c.
//
// Also picked up by the generic test_dynamic_lib_with_rewriter/
// test_static_exec_with_rewriter sweeps (which run every *.c in this
// directory standalone, with no --resume-from), so it must succeed on its
// own too -- which it does, since it only ever writes.

#include "helpers.h"

int main(void) {
    const char *path = "/tmp/lb_persist_test.txt";
    const char *marker = "persisted-across-runs";

    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    TEST_ASSERT(fd >= 0, "open for write failed");
    ssize_t n = write(fd, marker, strlen(marker));
    TEST_ASSERT(n == (ssize_t)strlen(marker), "write failed");
    TEST_ASSERT(close(fd) == 0, "close failed");

    printf("wrote\n");
    return 0;
}
