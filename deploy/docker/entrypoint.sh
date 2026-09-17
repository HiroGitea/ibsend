#!/bin/sh
# 容器入口：挑一份能执行的 ibsend。
#
# 镜像里有两份二进制。ibsend-ipc-lock 带着 cap_ipc_lock+ep 文件能力，非 root
# 用户靠它才能不受 memlock 限制地 pin 内存；可运行时没给 IPC_LOCK（边界集里
# 没有）时，内核会直接拒绝执行这个文件，只报一句 EPERM，让人摸不着头脑。
# 所以先看边界集：有 IPC_LOCK 就用带能力的那份，没有就退回普通的那份，
# 由 ibsend 自己说明该怎么配置。
set -eu

lib=/usr/local/lib/ibsend
bnd=$(sed -n 's/^CapBnd:[[:space:]]*//p' /proc/self/status)
if [ -n "$bnd" ] && [ $(( (0x$bnd >> 14) & 1 )) -eq 1 ]; then # CAP_IPC_LOCK = 14
    exec "$lib/ibsend-ipc-lock" "$@"
fi
exec "$lib/ibsend" "$@"
