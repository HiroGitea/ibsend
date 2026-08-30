/* ibx —— InfiniBand 文件传输数据面
 *
 * 数据搬运：接收端注册一大块内存池，发送端用 RDMA_WRITE_WITH_IMM 直接写进去，
 * immediate 携带该块的字节数。接收端 CPU 完全不参与数据搬运。块序号由 RC 的
 * 顺序保证隐式推导，所以 imm 32 位全用来放长度，既字节精确又支持流式。
 * imm == 0 表示传输结束。
 *
 * 控制协商（清单、续传点、校验和）走同一条 QP 上的 SEND/RECV，不再需要另开
 * TCP。好处有二：
 *   - 防火墙管不着。IPoIB 在内核眼里是普通网卡，TCP 要走完整 netfilter；而
 *     RDMA 完全绕开内核网络栈，iptables/ufw 对它无能为力。
 *   - 顺序理顺了。原先必须在连接**之前**就协商好 slab 尺寸（因为池子得在
 *     rdma_connect 前注册），只能把池子信息塞进 rdma_cm 的 private_data。
 *     现在流程是「先建连 → 再协商 → 再注册」，那个先有鸡还是先有蛋的问题
 *     自然消失。
 *
 * 唯一还沾 IP 的地方是 rdma_resolve_addr 把 IPoIB 地址解析成 GID，那一步走
 * ARP／邻居解析，不是 TCP，防火墙拦不到。
 *
 * 流控：接收端把「已释放块数」用 8 字节 inline SEND 回传，发送端据此维护信用
 * 窗口，窗口宽度等于整个内存池。磁盘慢的时候数据先堆在 RAM 里，只有池子真正
 * 写满才会阻塞发送端。
 */
#ifndef IBX_H
#define IBX_H

#include <stddef.h>
#include <stdint.h>

#define IBX_ERRLEN 256

typedef struct ibx ibx;

/* ================= 接收端 ================= */

/* 绑定端口，并在后台线程开始分配 + 注册 pool_bytes 字节的内存池。立即返回。
 *
 * bind_ip 必须是具体地址（不能是 0.0.0.0）：只有绑定到具体地址，rdma_cm 才能
 * 在此刻就确定用哪块 HCA，注册才能提前到等待连接之前发生。 */
ibx *ibx_listen_start(const char *bind_ip, uint16_t port, size_t pool_bytes, char *err);

/* 查询后台注册进度。1 = 就绪，0 = 进行中，-1 = 失败。
 * done/total 为已预取/总字节数（预取占绝大部分时间）。 */
int ibx_pool_progress(ibx *x, uint64_t *done, uint64_t *total, char *err);

/* 等待一个发送端连上来并完成 QP 建立。0 = 已连接，2 = 超时，-1 = 错误。
 *
 * 超时不是可有可无的：发送端可能在建连之后、发数据之前死掉。没有超时的话守护
 * 进程会永远卡在 rdma_get_cm_event 上，一个客户端就能挂死整个服务。 */
int ibx_listen_accept(ibx *x, int timeout_ms, char *err);

/* 控制协商完成后，按约定的切分方案武装数据面。
 * slab_size * nslabs 必须 <= 池子大小。池子本身早就注册好了，切分是纯算术。 */
int ibx_recv_arm(ibx *x, uint32_t slab_size, uint32_t nslabs, char *err);

/* 已注册内存池的远端可写地址与 rkey，通过控制通道告诉发送端 */
uint64_t ibx_pool_addr(const ibx *x);
uint32_t ibx_pool_rkey(const ibx *x);

/* 等待下一个到达的块，最多等 timeout_ms 毫秒（<0 无限等）。
 * 1 = 有数据，0 = 对端已结束，2 = 超时，-1 = 错误。
 *
 * 超时同样不是可选的：落盘是异步的，主循环必须能定期醒来归还已落盘的 slab。
 * 否则发送端等信用、接收端等新块，双方死锁。 */
int ibx_recv_next(ibx *x, void **buf, size_t *len, uint64_t *seq,
                  int timeout_ms, char *err);

/* 把阻塞在 ibx_recv_next 里的接收循环叫醒。
 *
 * **这是唯一一个可以从别的线程调用的函数**：它只往一个 eventfd 写 8 字节，
 * 不触碰任何 verbs 对象。落盘线程写完一块之后调用它，主循环就能立刻去归还
 * slab、补发信用，而不必等超时——靠超时"发现"落盘完成会在信用窗口紧张时
 * 退化成每块一个超时周期的锁步。 */
void ibx_recv_wake(ibx *x);

/* 该块已落盘，把 slab 归还给信用窗口。
 *
 * 注意：libibverbs 的 QP 操作不是线程安全的，所有 ibx_* 调用必须在同一线程。
 * 落盘线程应把「第几块写完了」回传给主线程，由主线程调用本函数。 */
int ibx_recv_release(ibx *x, uint64_t seq, char *err);

/* 结束本次传输：拆掉 QP 和这条连接，但**保留已注册的内存池**，之后可以再次
 * ibx_listen_accept 服务下一个发送端。守护进程因此只在启动时付一次注册代价。 */
int ibx_session_end(ibx *x, char *err);

/* ================= 发送端 ================= */

/* 连接接收端并建好 QP。失败时 *again 置 1 表示「对端还没就位」值得重试，
 * 置 0 表示本地配置或资源问题，重试一万次也不会成功。不区分这两者的话，
 * 一个必然失败的错误会被重试循环拖成静默挂起。
 *
 * resolve_ms 是地址解析和建连各阶段的超时（<=0 用默认 5 秒）。扫描子网做发现
 * 时要调小，否则每个空地址都要等满 ARP 超时。 */
ibx *ibx_connect(const char *peer_ip, uint16_t port, int resolve_ms,
                 int *again, char *err);

/* 控制协商完成后，注册本地池并记下对端池的位置。
 * local_slabs 只需够流水线，16 个足矣，至少 2 个。 */
int ibx_send_arm(ibx *x, uint32_t slab_size, uint32_t local_slabs,
                 uint64_t peer_addr, uint32_t peer_rkey, uint32_t peer_nslabs, char *err);

/* 领一个可写的本地 slab，阻塞直到本地 slab 空闲且对端有信用。
 * 返回缓冲区指针（4KB 对齐，可直接 O_DIRECT 读入），*seq 为块序号。
 *
 * 可以在提交之前连续领出多块——领和交是分开计数的——这样读盘可以交给别的
 * 线程并行进行，不必卡住发送循环。 */
void *ibx_send_acquire(ibx *x, uint64_t *seq, char *err);

/* 把 slab 前 len 字节 WRITE 到对端。len == 0 表示流结束。
 *
 * **必须按 seq 递增顺序调用**：接收端的块序号是靠 RC 的到达顺序隐式推导的，
 * 乱序提交会让文件内容错位。 */
int ibx_send_commit(ibx *x, uint64_t seq, size_t len, char *err);

/* 等待所有在途 WR 完成 */
int ibx_send_drain(ibx *x, char *err);

/* ================= 控制通道（两端通用） ================= */

/* 在 QP 上收发一条控制消息。长消息自动分帧，RC 保证可靠有序。 */
int ibx_ctl_send(ibx *x, const void *buf, uint32_t len, char *err);
/* 0 = 收到（*out_len 有效），2 = 超时，-1 = 错误 */
int ibx_ctl_recv(ibx *x, void *buf, uint32_t cap, uint32_t *out_len,
                 int timeout_ms, char *err);

/* ================= 杂项 ================= */

void     ibx_close(ibx *x);
uint64_t ibx_bytes(const ibx *x);       /* 本次会话累计传输字节 */
uint64_t ibx_pool_bytes(const ibx *x);
uint32_t ibx_slab_size(const ibx *x);
uint32_t ibx_nslabs(const ibx *x);
uint64_t ibx_inflight(const ibx *x);    /* 接收端：已收但未落盘的块数 */

/* 枚举本机的 IPoIB 接口（/sys/class/net/X/type == 32）及其 IPv4 地址，
 * 每行 "接口名 地址 掩码"。返回接口数，-1 出错。 */
int ibx_list_ipoib(char *buf, size_t buflen);

#endif /* IBX_H */
