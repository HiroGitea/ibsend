#!/bin/sh
# 从 Helm chart 重新生成本目录下的纯 YAML（ibsend-hostnetwork.yaml、ibsend-multus.yaml）。
# 改了 deploy/helm/ibsend 之后运行：
#   deploy/kubernetes/render.sh
# 需要 helm；不在 PATH 里时用 HELM=/path/to/helm 指定。
set -eu

cd "$(dirname "$0")/../.."
helm=${HELM:-helm}

# render <输出文件> <部署说明> [helm 参数...]
render() {
    out=$1
    apply=$2
    shift 2
    {
        printf '%s\n' \
            "# 由 deploy/helm/ibsend 生成，不要手改；改完 chart 运行 deploy/kubernetes/render.sh。" \
            "# 等价于：helm template ibsend deploy/helm/ibsend --namespace ibsend${*:+ $*}" \
            "#" \
            "# 部署：kubectl apply $apply" \
            "# 镜像默认是 ibsend:<appVersion>，先改成你推送到镜像仓库的地址。"
        "$helm" template ibsend deploy/helm/ibsend --namespace ibsend "$@"
    } > "$out.tmp"
    mv "$out.tmp" "$out"
}

render deploy/kubernetes/ibsend-hostnetwork.yaml \
    "-f deploy/kubernetes/namespace.yaml -f deploy/kubernetes/ibsend-hostnetwork.yaml"
render deploy/kubernetes/ibsend-multus.yaml \
    "-f deploy/kubernetes/namespace.yaml -f deploy/kubernetes/prerequisites/ipoib-network.yaml -f deploy/kubernetes/ibsend-multus.yaml" \
    --set network.mode=multus --set network.multus.networks=ipoib-network
