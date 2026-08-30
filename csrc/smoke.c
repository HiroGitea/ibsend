/* 环回冒烟测试：验证握手、WRITE_WITH_IMM、信用回流、数据完整性 */
#define _GNU_SOURCE
#include "ibx.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define SLAB   (1u << 20)   /* 1MB —— 让池子塞进 8MB 的 memlock 限制 */
#define NSLABS 4
#define CHUNKS 64           /* 传 64MB，会绕池子 16 圈，能压到信用流控 */

static uint8_t pattern(uint64_t seq, size_t i) { return (uint8_t)(seq * 31 + i * 7); }

static double now(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

int main(int argc, char **argv)
{
    char err[IBX_ERRLEN] = {0};
    if (argc < 3) { fprintf(stderr, "用法: smoke <server|client> <ip>\n"); return 2; }

    if (!strcmp(argv[1], "server")) {
        ibx *x = ibx_listen_start(argv[2], 18515, (size_t)SLAB * NSLABS, err);
        if (!x) { fprintf(stderr, "listen_start: %s\n", err); return 1; }
        if (ibx_listen_accept(x, SLAB, NSLABS, -1, err) < 0) { fprintf(stderr, "accept: %s\n", err); return 1; }
        fprintf(stderr, "[srv] 已连接, slab=%u nslabs=%u\n", ibx_slab_size(x), ibx_nslabs(x));

        void *buf; size_t len; uint64_t seq; uint64_t got = 0; int bad = 0;
        double t0 = now();
        int r;
        while ((r = ibx_recv_next(x, &buf, &len, &seq, -1, err)) == 1) {
            for (size_t i = 0; i < len; i += 4093)          /* 抽样校验 */
                if (((uint8_t *)buf)[i] != pattern(seq, i)) { bad++; break; }
            got += len;
            if (ibx_recv_release(x, seq, err) < 0) { fprintf(stderr, "release: %s\n", err); return 1; }
        }
        double dt = now() - t0;
        if (r < 0) { fprintf(stderr, "recv: %s\n", err); return 1; }
        fprintf(stderr, "[srv] 收到 %.1f MB, 校验错误块 %d, 用时 %.3fs (%.2f GB/s)\n",
                got / 1048576.0, bad, dt, got / dt / 1e9);
        ibx_close(x);
        return bad ? 1 : 0;
    }

    int again = 0;
    ibx *x = ibx_connect(argv[2], 18515, SLAB, NSLABS, &again, err);
    if (!x) { fprintf(stderr, "connect: %s\n", err); return 1; }
    fprintf(stderr, "[cli] 已连接, slab=%u\n", ibx_slab_size(x));

    for (int c = 0; c < CHUNKS; c++) {
        uint64_t seq;
        void *buf = ibx_send_acquire(x, &seq, err);
        if (!buf) { fprintf(stderr, "acquire: %s\n", err); return 1; }
        for (size_t i = 0; i < SLAB; i++) ((uint8_t *)buf)[i] = pattern(seq, i);
        if (ibx_send_commit(x, seq, SLAB, err) < 0) { fprintf(stderr, "commit: %s\n", err); return 1; }
    }
    uint64_t seq;
    if (!ibx_send_acquire(x, &seq, err)) { fprintf(stderr, "acquire eof: %s\n", err); return 1; }
    if (ibx_send_commit(x, seq, 0, err) < 0) { fprintf(stderr, "eof: %s\n", err); return 1; }
    if (ibx_send_drain(x, err) < 0) { fprintf(stderr, "drain: %s\n", err); return 1; }
    fprintf(stderr, "[cli] 发送完成 %.1f MB\n", ibx_bytes(x) / 1048576.0);
    ibx_close(x);
    return 0;
}
