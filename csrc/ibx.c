#define _GNU_SOURCE
#include "ibx.h"

#include <endian.h>
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <arpa/inet.h>
#include <ifaddrs.h>
#include <net/if.h>

#include <rdma/rdma_cma.h>
#include <infiniband/verbs.h>

#define HUGE_SZ    (2ul << 20)
#define CRED_SLOTS 64
#define IBX_MAGIC  0x49425831u  /* "IBX1" */

/* 控制通道：QP 上的小消息，用来替代原先那条 TCP。
 * RC 保证可靠有序，所以只需要把长消息切成帧再拼回去。
 * rnr_retry_count = 7（无限重试）意味着发送方跑到接收方前面时由硬件挂起重试，
 * 软件这边不需要额外流控。 */
/* 关键约束：RC QP 上接收 WR 是按**投递顺序**被消费的，跟消息类型无关。
 * 所以同一个队列上所有接收 WR 必须是同一尺寸的缓冲区，否则一条大消息可能
 * 落进一个为小消息准备的槽里，直接 local length error。
 * 消息种类因此靠帧头的标志位区分，不能靠缓冲区大小区分。
 *
 * 4KB 一帧：接收槽总量是 (池子/1MB) 个，于是接收缓冲的开销约为池子的 0.4%。 */
#define CTL_SLOTS  8                /* 数据面之外额外多备的槽 */
#define CTL_SIZE   4096
#define CTL_HDR    8                /* u32 本帧载荷长度 | u32 标志 */
#define CTL_LAST   1u
#define CTL_CRED   2u               /* 这一帧是信用通告，由 C 层直接消费 */
#define CTL_PAY    (CTL_SIZE - CTL_HDR)
#define MAX_NSLABS 8192
#define MAX_LOCAL  64

/* wr_id 的高 4 位用来区分完成事件的来源：同一个 CQ 上现在混着数据 WRITE、
 * 信用 SEND/RECV 和控制 SEND/RECV，不打标就分不开 */
#define TAG_SHIFT     28
#define TAG_DATA      (0x1ull << TAG_SHIFT)
#define TAG_CTL_RECV  (0x2ull << TAG_SHIFT)
#define TAG_CTL_SEND  (0x3ull << TAG_SHIFT)
#define TAG_CRED_RECV (0x4ull << TAG_SHIFT)
#define TAG_CRED_SEND (0x5ull << TAG_SHIFT)
#define TAG_OF(w)     ((w) & (0xFull << TAG_SHIFT))
#define IDX_OF(w)     ((w) & ((1ull << TAG_SHIFT) - 1))

#define ERR(...) do { if (err) snprintf(err, IBX_ERRLEN, __VA_ARGS__); } while (0)
#define ERRNO(msg) ERR("%s: %s", msg, strerror(errno))

/* 接收端通过 rdma_cm private_data 告知发送端本次传输的池子布局 */
struct pool_info {
    uint32_t magic;
    uint32_t rkey;
    uint64_t addr;
    uint32_t slab_size;
    uint32_t nslabs;
    uint32_t max_nslabs;   /* QP 创建时定死的上限，由池子大小推出 */
} __attribute__((packed));

struct ibx {
    int is_server;

    struct rdma_event_channel *ec;
    struct rdma_cm_id         *listen_id, *id;
    struct ibv_context        *ctx;
    struct ibv_pd             *pd;
    struct ibv_comp_channel   *ch;
    struct ibv_cq             *cq;
    struct ibv_mr             *mr;

    void    *pool;
    size_t   pool_bytes;
    uint32_t slab_size;
    uint32_t nslabs;
    uint32_t max_nslabs;   /* QP 创建时定死的上限，由池子大小推出 */

    /* 后台注册：与等待发送端连接的时间重叠，把启动停顿藏掉 */
    pthread_t        reg_thr;
    int              reg_running;
    atomic_int       reg_state;      /* 0=进行中 1=就绪 -1=失败 */
    atomic_ullong    reg_done;       /* 已预取字节数 */
    char             reg_err[IBX_ERRLEN];

    /* ---- 接收端 ---- */
    uint64_t  recv_seq, released, advertised;
    int       eof;
    uint64_t  cred_out;

    /* ---- 控制通道（与数据面共用接收队列，尺寸必须统一）---- */
    char          *ctl_buf;      /* nrecv 个接收槽 + 1 个发送槽 */
    struct ibv_mr *ctl_mr;
    uint32_t       nrecv;        /* 接收槽总数 */
    uint32_t      *ctl_q;        /* 已到达待消费的控制帧槽号，FIFO */
    uint32_t       ctl_qcap, ctl_qh, ctl_qt;
    uint64_t       ctl_send_out;

    /* 到达的 WRITE_WITH_IMM 也要排队：轮询 CQ 的地方不止一处
     * （ibx_recv_release 补发信用时也会轮询），不排队就会被别处顺手吃掉。 */
    uint32_t       imm_q[1024];
    uint32_t       imm_qh, imm_qt;

    /* 落盘线程用它把阻塞中的接收循环叫醒。
     * 没有它就只能靠超时去"发现"落盘完成——信用窗口紧张时会退化成每块一个
     * 超时周期的锁步，实测把吞吐压到了几百 MB/s。 */
    int            wake_fd;

    /* ---- 发送端 ---- */
    uint64_t  peer_addr;
    uint32_t  peer_rkey, peer_nslabs;
    /* reserved：已被 acquire 领走的块数；committed：已 post 出去的块数。
     * 两者必须分开，否则连续 acquire 会反复拿到同一块，读线程就没法预取。 */
    uint64_t  reserved, committed, completed, credit;

    uint64_t bytes;
};

/* ---------------- 内存池：2MB 对齐 + THP ---------------- */

static void *alloc_pool(size_t want, size_t *got)
{
    size_t sz = (want + HUGE_SZ - 1) & ~(HUGE_SZ - 1);
    char *raw = mmap(NULL, sz + HUGE_SZ, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (raw == MAP_FAILED)
        return NULL;

    char *aligned = (char *)(((uintptr_t)raw + HUGE_SZ - 1) & ~(uintptr_t)(HUGE_SZ - 1));
    if (aligned > raw)
        munmap(raw, aligned - raw);
    char *tail = aligned + sz, *rawend = raw + sz + HUGE_SZ;
    if (rawend > tail)
        munmap(tail, rawend - tail);

    /* mlx4 的 MTT 缓存很小，2MB 大页能把地址翻译表项数除以 512 */
    madvise(aligned, sz, MADV_HUGEPAGE);
    *got = sz;
    return aligned;
}

/* ---------------- 后台注册线程 ---------------- */

static void *reg_worker(void *arg)
{
    ibx *x = arg;

    /* 先把页预取出来（这一步占绝大部分时间，且能报进度），
     * 之后 ibv_reg_mr 的 get_user_pages 走在已驻留的页上会快很多 */
    for (size_t off = 0; off < x->pool_bytes; off += HUGE_SZ) {
        ((volatile char *)x->pool)[off] = 0;
        atomic_store_explicit(&x->reg_done, off + HUGE_SZ, memory_order_relaxed);
    }
    atomic_store_explicit(&x->reg_done, x->pool_bytes, memory_order_relaxed);

    x->mr = ibv_reg_mr(x->pd, x->pool, x->pool_bytes,
                       IBV_ACCESS_LOCAL_WRITE | IBV_ACCESS_REMOTE_WRITE);
    if (!x->mr) {
        snprintf(x->reg_err, IBX_ERRLEN, "ibv_reg_mr(%.1f GB) 失败：%s%s",
                 x->pool_bytes / 1073741824.0, strerror(errno),
                 errno == ENOMEM ? "  —— 多半是 ulimit -l 太小，需要 memlock unlimited" : "");
        atomic_store_explicit(&x->reg_state, -1, memory_order_release);
        return NULL;
    }
    atomic_store_explicit(&x->reg_state, 1, memory_order_release);
    return NULL;
}

int ibx_pool_progress(ibx *x, uint64_t *done, uint64_t *total, char *err)
{
    if (done)  *done  = atomic_load_explicit(&x->reg_done, memory_order_relaxed);
    if (total) *total = x->pool_bytes;
    int st = atomic_load_explicit(&x->reg_state, memory_order_acquire);
    if (st < 0) ERR("%s", x->reg_err);
    return st;
}

static int reg_join(ibx *x, char *err)
{
    if (x->reg_running) {
        pthread_join(x->reg_thr, NULL);
        x->reg_running = 0;
    }
    if (atomic_load_explicit(&x->reg_state, memory_order_acquire) < 0) {
        ERR("%s", x->reg_err);
        return -1;
    }
    return 0;
}

/* ---------------- CM 事件 ---------------- */

/* 返回 0 成功，2 超时（仅当 timeout_ms >= 0），-1 错误 */
static int wait_cm_to(ibx *x, enum rdma_cm_event_type want, struct rdma_cm_id **id_out,
                      struct pool_info *pd_out, int timeout_ms, char *err)
{
    if (timeout_ms >= 0) {
        struct pollfd pfd = { .fd = x->ec->fd, .events = POLLIN };
        int pr = poll(&pfd, 1, timeout_ms);
        if (pr < 0) { ERRNO("poll(cm 事件通道)"); return -1; }
        if (pr == 0) return 2;
    }
    struct rdma_cm_event *ev;
    if (rdma_get_cm_event(x->ec, &ev)) { ERRNO("rdma_get_cm_event"); return -1; }
    if (ev->event != want) {
        ERR("期望 CM 事件 %s，收到 %s (status=%d)",
            rdma_event_str(want), rdma_event_str(ev->event), ev->status);
        rdma_ack_cm_event(ev);
        return -1;
    }
    if (id_out) *id_out = ev->id;
    if (pd_out) {
        /* private_data 在 ack 时被释放，必须先拷出来 */
        if (ev->param.conn.private_data_len < sizeof(*pd_out)) {
            ERR("private_data 太短：%u < %zu", ev->param.conn.private_data_len, sizeof(*pd_out));
            rdma_ack_cm_event(ev);
            return -1;
        }
        memcpy(pd_out, ev->param.conn.private_data, sizeof(*pd_out));
    }
    rdma_ack_cm_event(ev);
    return 0;
}

static int wait_cm(ibx *x, enum rdma_cm_event_type want,
                   struct rdma_cm_id **id_out, struct pool_info *pd_out, char *err)
{
    return wait_cm_to(x, want, id_out, pd_out, -1, err);
}

/* ---------------- QP / CQ ---------------- */

static int setup_verbs(ibx *x, uint32_t max_send, uint32_t max_recv, char *err)
{
    struct ibv_device_attr da;
    if (ibv_query_device(x->id->verbs, &da)) { ERRNO("ibv_query_device"); return -1; }
    if (max_send > (uint32_t)da.max_qp_wr || max_recv > (uint32_t)da.max_qp_wr) {
        ERR("队列深度超限：需要 send=%u recv=%u，max_qp_wr=%d", max_send, max_recv, da.max_qp_wr);
        return -1;
    }
    if (x->pd && x->pd->context != x->id->verbs) {
        ERR("内存池注册在另一块 HCA 上（多网卡场景请指定 --bind 到目标网卡的 IP）");
        return -1;
    }
    if (!x->pd) {
        x->pd = ibv_alloc_pd(x->id->verbs);
        if (!x->pd) { ERRNO("ibv_alloc_pd"); return -1; }
    }

    x->ch = ibv_create_comp_channel(x->id->verbs);
    if (!x->ch) { ERRNO("ibv_create_comp_channel"); return -1; }

    int cqe = (int)(max_send + max_recv + 16);
    if (cqe > da.max_cqe) cqe = da.max_cqe;
    x->cq = ibv_create_cq(x->id->verbs, cqe, NULL, x->ch, 0);
    if (!x->cq) { ERRNO("ibv_create_cq"); return -1; }
    if (ibv_req_notify_cq(x->cq, 0)) { ERRNO("ibv_req_notify_cq"); return -1; }

    struct ibv_qp_init_attr qa = {
        .send_cq = x->cq, .recv_cq = x->cq, .qp_type = IBV_QPT_RC, .sq_sig_all = 0,
        .cap = { .max_send_wr = max_send, .max_recv_wr = max_recv,
                 .max_send_sge = 1, .max_recv_sge = 1, .max_inline_data = 64 },
    };
    if (rdma_create_qp(x->id, x->pd, &qa)) { ERRNO("rdma_create_qp"); return -1; }
    return 0;
}

/* WRITE_WITH_IMM 会消耗 responder 的一个 recv WR（但不用其 sge），
 * 所以接收端要 post 足够多的零长度 recv 来接住 immediate */
/* ---------------- CQ 轮询 ---------------- */

static int post_ctl_recv(ibx *x, uint32_t i, char *err);

/* 处理一个完成事件。所有来源都靠 wr_id 的标位区分——同一个 CQ 上混着数据
 * WRITE、信用 SEND/RECV 和控制 SEND/RECV，只看 opcode 是分不开的。 */
static int handle_wc(ibx *x, const struct ibv_wc *wc, char *err)
{
    if (wc->status != IBV_WC_SUCCESS) {
        /* RC QP 一旦出错会进 error 态，之后所有 WR 都被 flush */
        ERR("WR 失败：%s (opcode=%d, wr_id=0x%llx, vendor=0x%x)",
            ibv_wc_status_str(wc->status), wc->opcode,
            (unsigned long long)wc->wr_id, wc->vendor_err);
        return -1;
    }
    switch (TAG_OF(wc->wr_id)) {
    case TAG_DATA:      x->completed++;    return 0;
    case TAG_CTL_SEND:  x->ctl_send_out--; return 0;
    case TAG_CRED_SEND: x->cred_out--;     return 0;
    default:
        break;
    }

    /* 剩下的都是接收完成。WRITE_WITH_IMM 和 SEND 都消耗接收 WR，靠 opcode 分。 */
    uint32_t slot = (uint32_t)IDX_OF(wc->wr_id);
    if (wc->opcode == IBV_WC_RECV_RDMA_WITH_IMM) {
        const uint32_t QN = sizeof x->imm_q / sizeof x->imm_q[0];
        if (x->imm_qt - x->imm_qh >= QN) { ERR("数据块队列溢出"); return -1; }
        x->imm_q[x->imm_qt % QN] = ntohl(wc->imm_data);
        x->imm_qt++;
        return post_ctl_recv(x, slot, err);   /* 槽用完了立刻补回去 */
    }
    if (wc->opcode == IBV_WC_RECV) {
        char *sb = x->ctl_buf + (size_t)slot * CTL_SIZE;
        uint32_t fl;
        memcpy(&fl, sb + 4, 4);
        fl = ntohl(fl);
        if (fl & CTL_CRED) {
            /* 信用通告：C 层自己消费，不进控制帧队列 */
            uint64_t v;
            memcpy(&v, sb + CTL_HDR, sizeof v);
            x->credit = be64toh(v);
            return post_ctl_recv(x, slot, err);
        }
        if (x->ctl_qt - x->ctl_qh >= x->ctl_qcap) { ERR("控制帧队列溢出"); return -1; }
        x->ctl_q[x->ctl_qt % x->ctl_qcap] = slot;
        x->ctl_qt++;
        return 0;
    }
    return 0;
}

/* 超时的哨兵值。**不能用正数**：poll_cq_to 正常时返回「处理了几个完成事件」，
 * 用 2 表示超时的话，恰好批到 2 个事件的那一次就会被误读成超时——曾经因此
 * 出现间歇性握手卡死（GO 消息和第一个数据块被同一次轮询批到一起）。 */
#define POLL_TIMEOUT (-2)

/* timeout_ms: <0 无限等，>=0 超时返回 POLL_TIMEOUT。
 * 否则返回处理的完成数（>=0），出错返回 -1。 */
static int poll_cq_to(ibx *x, int timeout_ms, char *err)
{
    struct ibv_wc wc[16];
    int n = ibv_poll_cq(x->cq, 16, wc);
    if (n < 0) { ERR("ibv_poll_cq 失败"); return -1; }

    if (n == 0) {
        if (timeout_ms == 0) return 0;
        struct ibv_cq *ev_cq; void *ev_ctx;
        if (ibv_req_notify_cq(x->cq, 0)) { ERRNO("ibv_req_notify_cq"); return -1; }
        n = ibv_poll_cq(x->cq, 16, wc);   /* 关竞态窗口 */
        if (n < 0) { ERR("ibv_poll_cq 失败"); return -1; }
        if (n == 0) {
            struct pollfd pfd = { .fd = x->ch->fd, .events = POLLIN };
            int pr = poll(&pfd, 1, timeout_ms);
            if (pr < 0) {
                if (errno == EINTR) return 0;
                ERRNO("poll(完成通道)"); return -1;
            }
            if (pr == 0) return POLL_TIMEOUT;
            if (ibv_get_cq_event(x->ch, &ev_cq, &ev_ctx)) { ERRNO("ibv_get_cq_event"); return -1; }
            ibv_ack_cq_events(ev_cq, 1);
            n = ibv_poll_cq(x->cq, 16, wc);
            if (n < 0) { ERR("ibv_poll_cq 失败"); return -1; }
        }
    }
    for (int i = 0; i < n; i++)
        if (handle_wc(x, &wc[i], err) < 0) return -1;
    return n;
}

static int poll_cq(ibx *x, int blocking, char *err)
{
    return poll_cq_to(x, blocking ? -1 : 0, err);
}

/* ---------------- 控制通道 ---------------- */

static char *ctl_slot(ibx *x, uint32_t i) { return x->ctl_buf + (size_t)i * CTL_SIZE; }
static char *ctl_out(ibx *x)              { return x->ctl_buf + (size_t)x->nrecv * CTL_SIZE; }

static int post_ctl_recv(ibx *x, uint32_t i, char *err)
{
    struct ibv_sge sge = { .addr = (uintptr_t)ctl_slot(x, i),
                           .length = CTL_SIZE, .lkey = x->ctl_mr->lkey };
    struct ibv_recv_wr wr = { .wr_id = TAG_CTL_RECV | i, .sg_list = &sge, .num_sge = 1 }, *bad;
    if (ibv_post_recv(x->id->qp, &wr, &bad)) { ERRNO("ibv_post_recv(ctl)"); return -1; }
    return 0;
}

/* nrecv 个接收槽必须覆盖整个信用窗口：窗口有多宽，在途的 WRITE_WITH_IMM 就
 * 可能有多少，而每一个都会消耗一个接收 WR。 */
static int ctl_setup(ibx *x, uint32_t nrecv, char *err)
{
    x->nrecv = nrecv;
    x->ctl_qcap = nrecv;
    size_t sz = (size_t)(nrecv + 1) * CTL_SIZE;
    x->ctl_buf = calloc(1, sz);
    x->ctl_q = calloc(nrecv, sizeof(uint32_t));
    if (!x->ctl_buf || !x->ctl_q) { ERRNO("calloc(ctl)"); return -1; }
    x->ctl_mr = ibv_reg_mr(x->pd, x->ctl_buf, sz, IBV_ACCESS_LOCAL_WRITE);
    if (!x->ctl_mr) {
        ERR("ibv_reg_mr(接收槽 %.1f MB) 失败：%s", sz / 1048576.0, strerror(errno));
        return -1;
    }
    x->ctl_qh = x->ctl_qt = 0;
    for (uint32_t i = 0; i < nrecv; i++)
        if (post_ctl_recv(x, i, err) < 0) return -1;
    return 0;
}

static void ctl_teardown(ibx *x)
{
    if (x->ctl_mr) { ibv_dereg_mr(x->ctl_mr); x->ctl_mr = NULL; }
    free(x->ctl_buf);
    free(x->ctl_q);
    x->ctl_buf = NULL;
    x->ctl_q = NULL;
    x->nrecv = x->ctl_qcap = 0;
    x->ctl_qh = x->ctl_qt = 0;
}

static int ctl_send_flagged(ibx *x, const void *buf, uint32_t len, uint32_t extra, char *err)
{
    const char *p = buf;
    uint32_t left = len;
    do {
        uint32_t n = left > CTL_PAY ? CTL_PAY : left;
        /* 只有一个发送槽，复用之前必须等上一帧落地 */
        while (x->ctl_send_out > 0)
            if (poll_cq(x, 1, err) < 0) return -1;

        char *sb = ctl_out(x);
        uint32_t hn = htonl(n), hf = htonl((left == n ? CTL_LAST : 0) | extra);
        memcpy(sb, &hn, 4);
        memcpy(sb + 4, &hf, 4);
        if (n) memcpy(sb + CTL_HDR, p, n);

        struct ibv_sge sge = { .addr = (uintptr_t)sb, .length = CTL_HDR + n,
                               .lkey = x->ctl_mr->lkey };
        struct ibv_send_wr wr = { .wr_id = TAG_CTL_SEND, .sg_list = &sge, .num_sge = 1,
                                  .opcode = IBV_WR_SEND, .send_flags = IBV_SEND_SIGNALED }, *bad;
        if (ibv_post_send(x->id->qp, &wr, &bad)) { ERRNO("ibv_post_send(ctl)"); return -1; }
        x->ctl_send_out++;
        p += n;
        left -= n;
    } while (left > 0);

    while (x->ctl_send_out > 0)
        if (poll_cq(x, 1, err) < 0) return -1;
    return 0;
}

int ibx_ctl_send(ibx *x, const void *buf, uint32_t len, char *err)
{
    return ctl_send_flagged(x, buf, len, 0, err);
}

int ibx_ctl_recv(ibx *x, void *buf, uint32_t cap, uint32_t *out_len,
                 int timeout_ms, char *err)
{
    char *p = buf;
    uint32_t got = 0;
    for (;;) {
        while (x->ctl_qh == x->ctl_qt) {
            int r = poll_cq_to(x, timeout_ms, err);
            if (r == POLL_TIMEOUT) return 2;
            if (r < 0) return -1;
        }
        /* 取模必须和入队用同一个容量：入队是 % ctl_qcap（接收槽总数），
         * 这里曾经误用常量 CTL_SLOTS(8)，队列深度一超过 8 就读错槽位——
         * 多帧的大清单正好会触发。 */
        uint32_t slot = x->ctl_q[x->ctl_qh % x->ctl_qcap];
        x->ctl_qh++;

        char *sb = ctl_slot(x, slot);
        uint32_t n, fl;
        memcpy(&n, sb, 4);      n  = ntohl(n);
        memcpy(&fl, sb + 4, 4); fl = ntohl(fl);
        if (n > CTL_PAY) { ERR("控制帧长度非法：%u", n); return -1; }
        if (got + n > cap) { ERR("控制消息超出缓冲（%u 字节）", cap); return -1; }
        if (n) memcpy(p + got, sb + CTL_HDR, n);
        got += n;

        if (post_ctl_recv(x, slot, err) < 0) return -1;
        if (fl & CTL_LAST) break;
    }
    *out_len = got;
    return 0;
}

/* ---------------- 接收端：建立连接 ---------------- */

ibx *ibx_listen_start(const char *bind_ip, uint16_t port, size_t pool_bytes, char *err)
{
    ibx *x = calloc(1, sizeof(*x));
    if (!x) { ERRNO("calloc"); return NULL; }
    x->is_server = 1;
    x->wake_fd = -1;
    atomic_init(&x->reg_state, 0);
    atomic_init(&x->reg_done, 0);

    if (!bind_ip || !*bind_ip) { ERR("必须绑定到具体地址才能提前确定 HCA"); goto fail; }

    x->ec = rdma_create_event_channel();
    if (!x->ec) { ERRNO("rdma_create_event_channel"); goto fail; }
    if (rdma_create_id(x->ec, &x->listen_id, NULL, RDMA_PS_TCP)) { ERRNO("rdma_create_id"); goto fail; }

    struct sockaddr_in sa = { .sin_family = AF_INET, .sin_port = htons(port) };
    if (inet_pton(AF_INET, bind_ip, &sa.sin_addr) != 1) { ERR("bind 地址非法：%s", bind_ip); goto fail; }
    /* 端口被占时重试几次：授权后重启的场景里，新旧两个进程会短暂并存 */
    int bound = 0;
    for (int i = 0; i < 10; i++) {
        if (!rdma_bind_addr(x->listen_id, (struct sockaddr *)&sa)) { bound = 1; break; }
        if (errno != EADDRINUSE) break;
        struct timespec ts = { .tv_sec = 0, .tv_nsec = 300000000 };
        nanosleep(&ts, NULL);
    }
    if (!bound) { ERRNO("rdma_bind_addr"); goto fail; }
    if (rdma_listen(x->listen_id, 1)) { ERRNO("rdma_listen"); goto fail; }

    /* 绑定到具体地址后 rdma_cm 已经确定了设备，这样才能在等连接之前就注册 */
    x->ctx = x->listen_id->verbs;
    if (!x->ctx) { ERR("rdma_bind_addr 未能确定 HCA（%s 不是 IPoIB 地址？）", bind_ip); goto fail; }
    x->pd = ibv_alloc_pd(x->ctx);
    if (!x->pd) { ERRNO("ibv_alloc_pd"); goto fail; }

    x->wake_fd = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    if (x->wake_fd < 0) { ERRNO("eventfd"); goto fail; }

    x->pool = alloc_pool(pool_bytes, &x->pool_bytes);
    if (!x->pool) { ERRNO("mmap 内存池"); goto fail; }

    if (pthread_create(&x->reg_thr, NULL, reg_worker, x) != 0) {
        ERR("pthread_create(注册线程) 失败"); goto fail;
    }
    x->reg_running = 1;
    return x;

fail:
    ibx_close(x);
    return NULL;
}

int ibx_listen_accept(ibx *x, int timeout_ms, char *err)
{
    if (reg_join(x, err) < 0) return -1;

    /* 超时时 x->id 还是 NULL、什么都没建，直接返回即可，下次还能接着 accept */
    int r = wait_cm_to(x, RDMA_CM_EVENT_CONNECT_REQUEST, &x->id, NULL, timeout_ms, err);
    if (r != 0) return r;

    /* QP 深度创建时就定死，而切分方案要等控制面协商完才知道，所以按「这个
     * 池子最多能切成多少块」来开——slab 最小 1MB，于是上限就是 池子/1MB。
     * 按 MAX_NSLABS 一刀切的话，一次探测连接也要开 8192 个 recv WR，太浪费。 */
    uint64_t maxn = x->pool_bytes >> 20;
    if (maxn < 16) maxn = 16;
    if (maxn > MAX_NSLABS) maxn = MAX_NSLABS;
    x->max_nslabs = (uint32_t)maxn;
    uint32_t nrecv = x->max_nslabs + CTL_SLOTS + 8;
    if (setup_verbs(x, CTL_SLOTS + 8, nrecv, err) < 0) return -1;
    if (ctl_setup(x, nrecv, err) < 0) return -1;

    /* 不再往 private_data 里塞池子信息——那个 hack 是为了绕开
     * 「必须在连接前知道 slab 尺寸」，现在控制面在 QP 上，不需要了。 */
    struct rdma_conn_param cp = {
        .responder_resources = 4, .initiator_depth = 4,
        .retry_count = 7, .rnr_retry_count = 7,
    };
    if (rdma_accept(x->id, &cp)) { ERRNO("rdma_accept"); return -1; }
    if (wait_cm(x, RDMA_CM_EVENT_ESTABLISHED, NULL, NULL, err) < 0) return -1;
    return 0;
}

int ibx_recv_arm(ibx *x, uint32_t slab_size, uint32_t nslabs, char *err)
{
    slab_size = (slab_size + 4095) & ~4095u;   /* O_DIRECT 要 4K 对齐 */
    if (slab_size == 0 || nslabs == 0 || nslabs > x->max_nslabs) {
        ERR("切分参数非法：%u × %u", nslabs, slab_size);
        return -1;
    }
    if ((uint64_t)slab_size * nslabs > x->pool_bytes) {
        ERR("切分超出池子：%u × %u > %zu", nslabs, slab_size, x->pool_bytes);
        return -1;
    }
    x->slab_size = slab_size;
    x->nslabs = nslabs;
    x->recv_seq = x->released = x->advertised = 0;
    x->eof = 0;
    (void)err;
    /* 接收槽在建连时就按 max_nslabs 备好了，这里无需再 post */
    return 0;
}

uint64_t ibx_pool_addr(const ibx *x) { return (uint64_t)(uintptr_t)x->pool; }
uint32_t ibx_pool_rkey(const ibx *x) { return x->mr ? x->mr->rkey : 0; }

int ibx_session_end(ibx *x, char *err)
{
    (void)err;
    if (!x->id)
        return 0;

    rdma_disconnect(x->id);

    /* 必须把 DISCONNECTED 之类的事件排干净，否则下一次 get_cm_event 会拿到
     * 这些陈旧事件而不是新的 CONNECT_REQUEST。万一顺手吞掉了一个新的
     * CONNECT_REQUEST，对端的 ibx_connect 会收到 REJECT 并按 again=1 重试，
     * 能自愈。 */
    for (int i = 0; i < 8; i++) {
        struct pollfd pfd = { .fd = x->ec->fd, .events = POLLIN };
        if (poll(&pfd, 1, 150) <= 0)
            break;
        struct rdma_cm_event *ev;
        if (rdma_get_cm_event(x->ec, &ev))
            break;
        int done = (ev->event == RDMA_CM_EVENT_DISCONNECTED);
        rdma_ack_cm_event(ev);
        if (done)
            break;
    }

    if (x->id->qp)
        rdma_destroy_qp(x->id);
    ctl_teardown(x);
    if (x->cq) { ibv_destroy_cq(x->cq); x->cq = NULL; }
    if (x->ch) { ibv_destroy_comp_channel(x->ch); x->ch = NULL; }
    rdma_destroy_id(x->id);
    x->id = NULL;

    /* 只重置会话状态；pool / pd / mr 原样保留给下一次传输 */
    x->imm_qh = x->imm_qt = 0;
    x->ctl_send_out = 0;
    x->recv_seq = x->released = x->advertised = 0;
    x->eof = 0;
    x->cred_out = 0;
    x->bytes = 0;
    x->slab_size = x->nslabs = 0;
    return 0;
}

int ibx_list_ipoib(char *buf, size_t buflen)
{
    struct ifaddrs *ifa, *p;
    if (getifaddrs(&ifa))
        return -1;

    size_t off = 0;
    int n = 0;
    for (p = ifa; p; p = p->ifa_next) {
        if (!p->ifa_addr || p->ifa_addr->sa_family != AF_INET)
            continue;
        if (!(p->ifa_flags & IFF_UP) || (p->ifa_flags & IFF_LOOPBACK))
            continue;

        /* ARPHRD_INFINIBAND == 32 */
        char path[IFNAMSIZ + 32], type[16] = {0};
        snprintf(path, sizeof(path), "/sys/class/net/%s/type", p->ifa_name);
        FILE *f = fopen(path, "r");
        if (!f)
            continue;
        int got = fgets(type, sizeof(type), f) != NULL;
        fclose(f);
        /* 手写十进制解析而不是 atoi：glibc 2.38 起会在 C23 模式下把 atoi
         * 重定向到 __isoc23_strtol，在新发行版上编出的二进制就跑不了老系统 */
        int t = 0;
        for (const char *q = type; *q >= '0' && *q <= '9'; q++)
            t = t * 10 + (*q - '0');
        if (!got || t != 32)
            continue;

        char ip[INET_ADDRSTRLEN] = {0}, mask[INET_ADDRSTRLEN] = "255.255.255.0";
        inet_ntop(AF_INET, &((struct sockaddr_in *)p->ifa_addr)->sin_addr, ip, sizeof(ip));
        if (p->ifa_netmask)
            inet_ntop(AF_INET, &((struct sockaddr_in *)p->ifa_netmask)->sin_addr, mask, sizeof(mask));

        int w = snprintf(buf + off, buflen - off, "%s %s %s\n", p->ifa_name, ip, mask);
        if (w < 0 || (size_t)w >= buflen - off)
            break;
        off += w;
        n++;
    }
    freeifaddrs(ifa);
    return n;
}

/* ---------------- 发送端：建立连接 ---------------- */

ibx *ibx_connect(const char *peer_ip, uint16_t port, int resolve_ms, int *again, char *err)
{
    if (again) *again = 0;
    ibx *x = calloc(1, sizeof(*x));
    if (!x) { ERRNO("calloc"); return NULL; }
    x->wake_fd = -1;
    atomic_init(&x->reg_state, 1);
    atomic_init(&x->reg_done, 0);

    x->ec = rdma_create_event_channel();
    if (!x->ec) { ERRNO("rdma_create_event_channel"); goto fail; }
    if (rdma_create_id(x->ec, &x->id, NULL, RDMA_PS_TCP)) { ERRNO("rdma_create_id"); goto fail; }

    struct sockaddr_in sa = { .sin_family = AF_INET, .sin_port = htons(port) };
    if (inet_pton(AF_INET, peer_ip, &sa.sin_addr) != 1) { ERR("对端地址非法：%s", peer_ip); goto fail; }

    if (again) *again = 1;   /* 以下是建连阶段，失败通常意味着对端还没就位 */
    /* rdma_resolve_addr 自动把 IPoIB 的 IP 解析成 GID。这一步走的是 ARP／邻居
     * 解析，不是 TCP，所以即使防火墙封了所有 TCP 入站，它照样能成。 */
    if (resolve_ms <= 0) resolve_ms = 5000;
    if (rdma_resolve_addr(x->id, NULL, (struct sockaddr *)&sa, resolve_ms)) { ERRNO("rdma_resolve_addr"); goto fail; }
    if (wait_cm_to(x, RDMA_CM_EVENT_ADDR_RESOLVED, NULL, NULL, resolve_ms + 500, err) != 0) goto fail;
    if (rdma_resolve_route(x->id, resolve_ms)) { ERRNO("rdma_resolve_route"); goto fail; }
    if (wait_cm_to(x, RDMA_CM_EVENT_ROUTE_RESOLVED, NULL, NULL, resolve_ms + 500, err) != 0) goto fail;
    if (again) *again = 0;   /* 以下是本地资源问题，重试没有意义 */

    if (setup_verbs(x, MAX_LOCAL + CTL_SLOTS + 8, CRED_SLOTS + 8, err) < 0) goto fail;

    /* 发送端只需要收信用通告和几条控制消息，槽位不用多 */
    if (ctl_setup(x, CRED_SLOTS, err) < 0) goto fail;

    struct rdma_conn_param cp = { .responder_resources = 4, .initiator_depth = 4,
                                  .retry_count = 7, .rnr_retry_count = 7 };
    if (again) *again = 1;
    if (rdma_connect(x->id, &cp)) { ERRNO("rdma_connect"); goto fail; }
    if (wait_cm_to(x, RDMA_CM_EVENT_ESTABLISHED, NULL, NULL, resolve_ms + 2000, err) != 0) goto fail;
    if (again) *again = 0;
    return x;

fail:
    ibx_close(x);
    return NULL;
}

int ibx_send_arm(ibx *x, uint32_t slab_size, uint32_t local_slabs,
                 uint64_t peer_addr, uint32_t peer_rkey, uint32_t peer_nslabs, char *err)
{
    if (local_slabs < 2) local_slabs = 2;
    if (local_slabs > MAX_LOCAL) local_slabs = MAX_LOCAL;
    if (slab_size == 0 || peer_nslabs == 0) { ERR("对端给的切分参数非法"); return -1; }

    x->slab_size   = slab_size;
    x->nslabs      = local_slabs;
    x->peer_addr   = peer_addr;
    x->peer_rkey   = peer_rkey;
    x->peer_nslabs = peer_nslabs;
    x->reserved = x->committed = x->completed = x->credit = 0;

    x->pool = alloc_pool((size_t)slab_size * local_slabs, &x->pool_bytes);
    if (!x->pool) { ERRNO("mmap 本地池"); return -1; }
    x->mr = ibv_reg_mr(x->pd, x->pool, x->pool_bytes, IBV_ACCESS_LOCAL_WRITE);
    if (!x->mr) {
        ERR("ibv_reg_mr(本地池 %.1f MB) 失败：%s%s", x->pool_bytes / 1048576.0,
            strerror(errno), errno == ENOMEM ? "  —— memlock 额度不够，见 ulimit -l" : "");
        return -1;
    }
    return 0;
}

/* ---------------- 发送端数据面 ---------------- */

void *ibx_send_acquire(ibx *x, uint64_t *seq, char *err)
{
    /* 两个约束：对端池不能溢出，本地 slab 不能覆盖还在飞的数据。
     * 用 reserved 而不是 committed 来判断，这样可以在提交之前先领出多块，
     * 交给读线程并行填充。 */
    while ((x->reserved - x->credit >= x->peer_nslabs) ||
           (x->reserved - x->completed >= x->nslabs))
        if (poll_cq(x, 1, err) < 0) return NULL;
    *seq = x->reserved;
    x->reserved++;
    return (char *)x->pool + (size_t)(*seq % x->nslabs) * x->slab_size;
}

int ibx_send_commit(ibx *x, uint64_t seq, size_t len, char *err)
{
    if (len > x->slab_size) { ERR("len %zu 超过 slab_size %u", len, x->slab_size); return -1; }
    struct ibv_sge sge = {
        .addr = (uintptr_t)x->pool + (size_t)(seq % x->nslabs) * x->slab_size,
        .length = (uint32_t)len, .lkey = x->mr->lkey,
    };
    struct ibv_send_wr wr = {
        .wr_id = TAG_DATA | (seq & (TAG_DATA - 1)), .sg_list = len ? &sge : NULL, .num_sge = len ? 1 : 0,
        .opcode = IBV_WR_RDMA_WRITE_WITH_IMM,
        .send_flags = IBV_SEND_SIGNALED,   /* 8MB 块 → 约 400 WR/s，全 signal 无所谓 */
        .imm_data = htonl((uint32_t)len),  /* len==0 即 EOF */
        .wr.rdma = { .remote_addr = x->peer_addr + (uint64_t)(seq % x->peer_nslabs) * x->slab_size,
                     .rkey = x->peer_rkey },
    }, *bad;
    if (ibv_post_send(x->id->qp, &wr, &bad)) { ERRNO("ibv_post_send"); return -1; }
    x->committed++;
    x->bytes += len;
    return 0;
}

int ibx_send_drain(ibx *x, char *err)
{
    while (x->completed < x->committed)
        if (poll_cq(x, 1, err) < 0) return -1;
    return 0;
}

/* ---------------- 接收端数据面 ---------------- */

int ibx_recv_next(ibx *x, void **buf, size_t *len, uint64_t *seq,
                  int timeout_ms, char *err)
{
    const uint32_t QN = sizeof x->imm_q / sizeof x->imm_q[0];
    if (x->eof) return 0;
    for (;;) {
        if (x->imm_qh != x->imm_qt) {
            uint32_t nb = x->imm_q[x->imm_qh % QN];
            x->imm_qh++;
            if (nb == 0) { x->eof = 1; return 0; }
            uint64_t s = x->recv_seq++;
            *seq = s;
            *len = nb;
            /* 数据已由网卡 DMA 落进池子，CPU 全程没碰过 */
            *buf = (char *)x->pool + (size_t)(s % x->nslabs) * x->slab_size;
            x->bytes += nb;
            return 1;
        }
        /* 先非阻塞地看一眼 CQ，有货就不用睡 */
        int r = poll_cq_to(x, 0, err);
        if (r == POLL_TIMEOUT) return 2;
        if (r < 0) return -1;
        if (r > 0) continue;

        /* 同时等两件事：网卡的完成事件，和落盘线程的唤醒 */
        struct pollfd pfd[2] = {
            { .fd = x->ch->fd,  .events = POLLIN },
            { .fd = x->wake_fd, .events = POLLIN },
        };
        if (ibv_req_notify_cq(x->cq, 0)) { ERRNO("ibv_req_notify_cq"); return -1; }
        r = poll_cq_to(x, 0, err);                  /* 关竞态窗口 */
        if (r == POLL_TIMEOUT) return 2;
        if (r < 0) return -1;
        if (x->imm_qh != x->imm_qt) continue;

        int pr = poll(pfd, x->wake_fd >= 0 ? 2 : 1, timeout_ms);
        if (pr < 0) {
            if (errno == EINTR) continue;
            ERRNO("poll(接收等待)"); return -1;
        }
        if (pr == 0) return 2;                      /* 超时 */
        if (pfd[0].revents & POLLIN) {
            struct ibv_cq *ev_cq; void *ev_ctx;
            if (ibv_get_cq_event(x->ch, &ev_cq, &ev_ctx)) { ERRNO("ibv_get_cq_event"); return -1; }
            ibv_ack_cq_events(ev_cq, 1);
            r = poll_cq_to(x, 0, err);
            if (r == POLL_TIMEOUT) return 2;
            if (r < 0) return -1;
        }
        if (x->wake_fd >= 0 && (pfd[1].revents & POLLIN)) {
            uint64_t v;
            while (read(x->wake_fd, &v, sizeof v) == (ssize_t)sizeof v) { }
            return 2;   /* 有 slab 待归还，让调用方去收割 */
        }
    }
}

void ibx_recv_wake(ibx *x)
{
    /* 从落盘线程调用。只是往 eventfd 写 8 字节，本身是线程安全的，
     * 不触碰任何 verbs 对象。 */
    if (!x || x->wake_fd < 0) return;
    uint64_t v = 1;
    ssize_t n = write(x->wake_fd, &v, sizeof v);
    (void)n;
}

int ibx_recv_release(ibx *x, uint64_t seq, char *err)
{
    (void)seq;
    x->released++;

    /* 攒够 1/16 个池子再通告，避免每块都发一个 credit */
    uint64_t step = x->nslabs / 16;
    if (step == 0) step = 1;
    if (x->released - x->advertised < step && x->released < x->recv_seq) return 0;

    /* 信用走和控制消息同一条帧通道——接收 WR 是按投递顺序消费的，混用不同
     * 尺寸的缓冲区会让大消息落进小槽里直接报长度错误。 */
    uint64_t val = htobe64(x->released);
    if (ctl_send_flagged(x, &val, sizeof val, CTL_CRED, err) < 0) return -1;
    x->advertised = x->released;
    return 0;
}

/* ---------------- 杂项 ---------------- */

void ibx_close(ibx *x)
{
    if (!x) return;
    if (x->reg_running) { pthread_join(x->reg_thr, NULL); x->reg_running = 0; }
    if (x->id && x->id->qp) rdma_destroy_qp(x->id);
    ctl_teardown(x);
    if (x->mr)      ibv_dereg_mr(x->mr);
    if (x->pool)    munmap(x->pool, x->pool_bytes);   /* 必须在 dereg 之后 */
    if (x->cq)      ibv_destroy_cq(x->cq);
    if (x->ch)      ibv_destroy_comp_channel(x->ch);
    if (x->pd)      ibv_dealloc_pd(x->pd);
    if (x->id)        rdma_destroy_id(x->id);
    if (x->listen_id) rdma_destroy_id(x->listen_id);
    if (x->ec)      rdma_destroy_event_channel(x->ec);
    if (x->wake_fd >= 0) close(x->wake_fd);
    free(x);
}

uint64_t ibx_bytes(const ibx *x)      { return x->bytes; }
uint64_t ibx_pool_bytes(const ibx *x) { return x->pool_bytes; }
uint32_t ibx_slab_size(const ibx *x)  { return x->slab_size; }
uint32_t ibx_nslabs(const ibx *x)     { return x->nslabs; }
uint64_t ibx_inflight(const ibx *x)   { return x->recv_seq - x->released; }
