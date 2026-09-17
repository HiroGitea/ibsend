# syntax=docker/dockerfile:1
#
# ibsend 容器镜像，只含 CLI。
#
#   docker build -t ibsend .
#
# 运行时需要 IPoIB 接口（--network host，或 Kubernetes 里 Multus 挂进来的）、
# /dev/infiniband 和 IPC_LOCK 能力，见 docs/containers.md。

ARG DEBIAN_RELEASE=bookworm

FROM rust:1-${DEBIAN_RELEASE} AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends libibverbs-dev librdmacm-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY csrc csrc
COPY src src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin ibsend \
 && install -m 0755 target/release/ibsend /ibsend

FROM debian:${DEBIAN_RELEASE}-slim
LABEL org.opencontainers.image.title="ibsend" \
      org.opencontainers.image.description="Fast file transfers over InfiniBand" \
      org.opencontainers.image.source="https://github.com/HiroGitea/ibsend" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

# ibverbs-providers 是各家 HCA 的用户态驱动（mlx4、mlx5……），
# 少了它 ibv_get_device_list 什么都找不到
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      libibverbs1 librdmacm1 ibverbs-providers tini libcap2-bin \
 && rm -rf /var/lib/apt/lists/*

# 两份二进制，由入口脚本挑选（原因见 entrypoint.sh）。文件能力挂在 inode 上，
# 所以必须是副本，不能是硬链接。
COPY --from=build /ibsend /usr/local/lib/ibsend/ibsend
RUN cp /usr/local/lib/ibsend/ibsend /usr/local/lib/ibsend/ibsend-ipc-lock \
 && setcap cap_ipc_lock+ep /usr/local/lib/ibsend/ibsend-ipc-lock \
 && apt-get purge -y --auto-remove libcap2-bin
COPY --chmod=0755 deploy/docker/entrypoint.sh /usr/local/bin/ibsend
COPY LICENSE-MIT LICENSE-APACHE /usr/share/doc/ibsend/

RUN groupadd --system --gid 10001 ibsend \
 && useradd --system --uid 10001 --gid 10001 --home-dir /data --no-create-home \
      --shell /usr/sbin/nologin ibsend \
 && install -d -o 10001 -g 10001 /data /run/ibsend

USER 10001:10001
WORKDIR /data
# tini 负责转发信号：ibsend 不是 PID 1，SIGTERM 才能照常结束它
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/ibsend"]
CMD ["daemon", "--out", "/data", "--ready-file", "/run/ibsend/ready"]
