#define _GNU_SOURCE
#include <fcntl.h>
#include <linux/stat.h>
#include <stdio.h>
#include <sys/stat.h>

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    struct statx result;
    if (statx(AT_FDCWD, argv[1], 0, STATX_DIOALIGN, &result)) {
        perror("statx");
        return 1;
    }
    printf("%u %u %u\n", !!(result.stx_mask & STATX_DIOALIGN),
           result.stx_dio_mem_align, result.stx_dio_offset_align);
    return 0;
}
