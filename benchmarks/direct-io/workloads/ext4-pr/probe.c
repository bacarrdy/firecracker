#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/aio_abi.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

enum { KIB = 1024, MIB = KIB * KIB, PAGE = 4 * KIB, SEQUENTIAL = 128 * MIB,
       MIXED = 64 * MIB, QD = 16, PAGES = MIXED / PAGE };

static void fail(const char *operation) {
    perror(operation);
    exit(2);
}

static double now(void) {
    struct timespec sample;
    if (clock_gettime(CLOCK_MONOTONIC, &sample)) fail("clock_gettime");
    return sample.tv_sec + sample.tv_nsec * 1e-9;
}

static void await_host(void) {
    if (getchar() != '\n') fail("host handshake");
}

struct measurement {
    double elapsed;
    double read_p99_us;
    double write_p99_us;
};

static int compare_latency(const void *a, const void *b) {
    double left = *(const double *)a;
    double right = *(const double *)b;
    return (left > right) - (left < right);
}

static double p99(double *latencies, unsigned count) {
    if (!count) return 0;
    qsort(latencies, count, sizeof(*latencies), compare_latency);
    return latencies[(count * 99 + 99) / 100 - 1];
}

static void report(const char *name, unsigned amount, struct measurement result) {
    printf("QA %s %.6f %.3f %.3f\n", name, amount / result.elapsed,
           result.read_p99_us, result.write_p99_us);
}

static uint64_t marker(unsigned generation, unsigned page) {
    return ((uint64_t)generation << 32) | page;
}

static void prepare_page(unsigned char *buffer, unsigned generation, unsigned page) {
    uint64_t header = marker(generation, page);
    memset(buffer, 0x31 + generation, PAGE);
    memcpy(buffer, &header, sizeof(header));
}

static void verify_pages(const unsigned char *buffer, unsigned start, unsigned pages,
                         const unsigned *order, unsigned generation) {
    unsigned char expected[PAGE];
    for (unsigned i = 0; i < pages; i++) {
        unsigned page = order ? order[i] : i;
        if (order && (i & 1)) continue;
        prepare_page(expected, generation, start + page);
        if (memcmp(expected, buffer + (size_t)page * PAGE, PAGE)) {
            fprintf(stderr, "QA data mismatch at page %u (generation %u)\n", start + page, generation);
            exit(2);
        }
    }
}

static struct measurement run_io(int fd, aio_context_t context, unsigned char *buffer, unsigned extent,
                     unsigned bytes, off_t offset, const unsigned *order, unsigned write_sequentially) {
    struct iocb requests[QD];
    struct iocb *submit[QD];
    struct io_event completed[QD];
    unsigned count = extent / bytes;
    double latencies[2][PAGES];
    unsigned samples[2] = {0, 0};
    double start = now();
    for (unsigned i = 0; i < count; i += QD) {
        memset(requests, 0, sizeof(requests));
        for (unsigned j = 0; j < QD; j++) {
            unsigned block = order ? order[i + j] : i + j;
            unsigned write_it = order ? ((i + j) & 1) : write_sequentially;
            struct iocb *request = submit[j] = &requests[j];
            request->aio_data = write_it;
            request->aio_lio_opcode = write_it ? IOCB_CMD_PWRITE : IOCB_CMD_PREAD;
            request->aio_fildes = fd;
            request->aio_buf = (uintptr_t)(buffer + (size_t)block * bytes);
            request->aio_nbytes = bytes;
            request->aio_offset = offset + (off_t)block * bytes;
        }
        unsigned sent = 0;
        double submitted = now();
        while (sent < QD) {
            long pushed = syscall(SYS_io_submit, context, QD - sent, submit + sent);
            if (pushed <= 0) fail("io_submit");
            sent += pushed;
        }
        unsigned done = 0;
        while (done < QD) {
            long received = syscall(SYS_io_getevents, context, 1, QD - done, completed, NULL);
            if (received <= 0) fail("io_getevents");
            double latency = (now() - submitted) * 1e6;
            for (long j = 0; j < received; j++) {
                if (completed[j].res != bytes || completed[j].res2 != 0) {
                    fprintf(stderr, "QA unexpected AIO result %lld/%lld, expected %u\n",
                            (long long)completed[j].res, (long long)completed[j].res2, bytes);
                    exit(2);
                }
                unsigned direction = completed[j].data;
                latencies[direction][samples[direction]++] = latency;
            }
            done += received;
        }
    }
    if ((write_sequentially || order) && fsync(fd)) fail("fsync");
    double elapsed = now() - start;
    return (struct measurement){
        elapsed, p99(latencies[0], samples[0]), p99(latencies[1], samples[1])
    };
}

static void shuffle(unsigned order[PAGES]) {
    uint32_t state = 0x849fe7ab;
    for (unsigned i = 0; i < PAGES; i++) order[i] = i;
    for (unsigned i = PAGES - 1; i > 0; i--) {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        unsigned j = state % (i + 1);
        unsigned saved = order[i];
        order[i] = order[j];
        order[j] = saved;
    }
}

int main(void) {
    if (setvbuf(stdout, NULL, _IOLBF, 0)) fail("setvbuf");
    int fd = open("/dev/vda", O_RDWR | O_DIRECT);
    if (fd < 0) fail("open /dev/vda");
    struct termios serial;
    if (tcgetattr(STDIN_FILENO, &serial)) fail("tcgetattr");
    serial.c_lflag &= ~ECHO;
    if (tcsetattr(STDIN_FILENO, TCSANOW, &serial)) fail("tcsetattr");
    aio_context_t context = 0;
    if (syscall(SYS_io_setup, QD, &context)) fail("io_setup");
    unsigned char *buffer;
    if (posix_memalign((void **)&buffer, PAGE, SEQUENTIAL)) fail("posix_memalign");
    memset(buffer, 0, SEQUENTIAL);

    puts("QA ready");
    await_host();
    struct measurement elapsed = run_io(fd, context, buffer, SEQUENTIAL, MIB, 0, NULL, 0);
    verify_pages(buffer, 0, SEQUENTIAL / PAGE, NULL, 0);
    report("cold-read-MiBs", SEQUENTIAL / MIB, elapsed);
    await_host();

    elapsed = run_io(fd, context, buffer, SEQUENTIAL, MIB, 0, NULL, 0);
    verify_pages(buffer, 0, SEQUENTIAL / PAGE, NULL, 0);
    report("warm-read-MiBs", SEQUENTIAL / MIB, elapsed);
    await_host();

    for (unsigned i = 0; i < SEQUENTIAL / PAGE; i++)
        prepare_page(buffer + (size_t)i * PAGE, 1, SEQUENTIAL / PAGE + i);
    elapsed = run_io(fd, context, buffer, SEQUENTIAL, MIB, SEQUENTIAL, NULL, 1);
    report("write-with-flush-MiBs", SEQUENTIAL / MIB, elapsed);
    await_host();

    unsigned order[PAGES];
    shuffle(order);
    for (unsigned i = 1; i < PAGES; i += 2)
        prepare_page(buffer + (size_t)order[i] * PAGE, 2, (2 * SEQUENTIAL) / PAGE + order[i]);
    elapsed = run_io(fd, context, buffer, MIXED, PAGE, 2 * SEQUENTIAL, order, 0);
    verify_pages(buffer, (2 * SEQUENTIAL) / PAGE, PAGES, order, 0);
    report("mixed-with-flush-IOPS", PAGES, elapsed);
    await_host();

    if (syscall(SYS_io_destroy, context)) fail("io_destroy");
    if (close(fd)) fail("close");
    free(buffer);
    puts("QA-verified");
    return 0;
}
