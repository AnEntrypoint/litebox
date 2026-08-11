// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

// Reads back the file persist_write.c writes, printing its contents (or
// "no data" if it doesn't exist -- e.g. when run standalone by the generic
// test_dynamic_lib_with_rewriter/test_static_exec_with_rewriter sweeps,
// which run every *.c in this directory with a fresh, empty /tmp and no
// --resume-from). See test_export_and_resume_writable_layer in run.rs.

#include "helpers.h"

int main(void) {
    const char *path = "/tmp/lb_persist_test.txt";

    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        TEST_ASSERT(errno == ENOENT, "unexpected open error");
        printf("no data\n");
        return 0;
    }

    char buf[128] = {0};
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    TEST_ASSERT(n >= 0, "read failed");
    TEST_ASSERT(close(fd) == 0, "close failed");

    printf("%s\n", buf);
    return 0;
}
