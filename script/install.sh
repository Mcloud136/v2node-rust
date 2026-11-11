#!/bin/bash
# v2node-rust installer
set -u

red='\033[0;31m'
green='\033[0;32m'
yellow='\033[0;33m'
plain='\033[0m'

SERVICE_NAME=${V2NODE_RUST_SERVICE:-v2node-rust}
INSTALL_DIR=${V2NODE_RUST_INSTALL_DIR:-/usr/local/v2node}
CONFIG_FILE=${V2NODE_RUST_CONFIG:-/etc/v2node-rust/config.json}
SYSTEMD_DIR=${V2NODE_RUST_SYSTEMD_DIR:-/etc/systemd/system}
OPENRC_DIR=${V2NODE_RUST_OPENRC_DIR:-/etc/init.d}
MANAGER_PATH=${V2NODE_RUST_MANAGER:-/usr/bin/v2node-rust}
REPO=${V2NODE_RUST_REPO:-Mcloud136/v2node-rust}
BRANCH=${V2NODE_RUST_BRANCH:-main}
SKIP_SERVICE=${V2NODE_RUST_SKIP_SERVICE:-0}

BIN="$INSTALL_DIR/v2node-rust"

usage() {
    cat <<EOF
用法: $0 [版本号] [选项]

选项:
  --local PATH        从本地二进制或目录安装（不联网）
  --url URL           从自定义 URL 下载 tar.gz 或二进制
  --version VERSION   指定版本（例如 0.1.0 或 v0.1.0）
  --api-host URL      面板 API 地址
  --node-id ID        节点 ID
  --api-key KEY       节点通讯密钥
  --config PATH       配置文件路径（默认 $CONFIG_FILE）
  --skip-service      只安装二进制/配置，不修改 systemd/OpenRC
  -h, --help          显示帮助

环境变量:
  V2NODE_RUST_REPO             GitHub 仓库（owner/repo）
  V2NODE_RUST_BRANCH           分支（默认 main）
  V2NODE_RUST_INSTALL_DIR      安装目录（默认 $INSTALL_DIR）
  V2NODE_RUST_CONFIG           配置文件（默认 $CONFIG_FILE）
  V2NODE_RUST_SERVICE          服务名（默认 $SERVICE_NAME）
EOF
}

die() {
    echo -e "${red}错误：${plain}$*" >&2
    exit 1
}

warn() {
    echo -e "${yellow}警告：${plain}$*"
}

detect_release() {
    if [ -f /etc/alpine-release ] || grep -qi "alpine" /etc/issue 2>/dev/null; then
        echo alpine
    elif [ -f /etc/redhat-release ]; then
        echo centos
    elif grep -qi "debian" /etc/issue 2>/dev/null || grep -qi "debian" /proc/version 2>/dev/null; then
        echo debian
    elif grep -qi "ubuntu" /etc/issue 2>/dev/null || grep -qi "ubuntu" /proc/version 2>/dev/null; then
        echo ubuntu
    elif grep -qi "arch" /proc/version 2>/dev/null; then
        echo arch
    else
        echo unknown
    fi
}

RELEASE=$(detect_release)

pkg_install() {
    case "$RELEASE" in
        alpine) apk add --no-cache "$@" >/dev/null 2>&1 ;;
        debian|ubuntu) DEBIAN_FRONTEND=noninteractive apt-get update -qq >/dev/null 2>&1 && DEBIAN_FRONTEND=noninteractive apt-get install -y "$@" >/dev/null 2>&1 ;;
        centos) yum install -y "$@" >/dev/null 2>&1 ;;
        arch) pacman -S --noconfirm --needed "$@" >/dev/null 2>&1 ;;
        *) return 1 ;;
    esac
}

ensure_deps() {
    if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
        pkg_install curl || die "需要 curl 或 wget，请先安装"
    fi
    if ! command -v tar >/dev/null 2>&1; then
        pkg_install tar || die "需要 tar，请先安装"
    fi
}

download() {
    local url=$1
    local output=$2
    if command -v curl >/dev/null 2>&1; then
        curl -fLsS --connect-timeout 10 --retry 3 --retry-delay 1 -o "$output" "$url"
    else
        wget -q --tries=3 --timeout=10 -O "$output" "$url"
    fi
}

fetch_text() {
    local url=$1
    if command -v curl >/dev/null 2>&1; then
        curl -fLsS --connect-timeout 10 "$url"
    else
        wget -qO- "$url"
    fi
}

json_escape() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

generate_config() {
    local file=$1
    local api_host=$2
    local node_id=$3
    local api_key=$4

    mkdir -p "$(dirname "$file")" || die "无法创建配置目录"
    local tmp
    tmp=$(mktemp "${file}.XXXXXX") || die "无法创建临时配置文件"
    cat > "$tmp" <<EOF
{
    "Log": {
        "Level": "warn",
        "Output": "",
        "Access": "none",
        "Ansi": false
    },
    "Nodes": [
        {
            "ApiHost": "$(json_escape "$api_host")",
            "NodeID": ${node_id},
            "ApiKey": "$(json_escape "$api_key")",
            "Timeout": 15
        }
    ]
}
EOF
    chmod 600 "$tmp" 2>/dev/null || true
    mv -f "$tmp" "$file"
    echo -e "${green}配置文件已生成：${plain}$file"
}

validate_api_args() {
    [ -n "$API_HOST_ARG" ] && [ -n "$NODE_ID_ARG" ] && [ -n "$API_KEY_ARG" ] || die "--api-host、--node-id、--api-key 必须同时提供"
    case "$API_HOST_ARG" in
        http://*|https://*) ;;
        *) die "面板 API 地址必须以 http:// 或 https:// 开头" ;;
    esac
    case "$NODE_ID_ARG" in
        ''|*[!0-9]*) die "节点 ID 必须是数字" ;;
    esac
    [ "$NODE_ID_ARG" -gt 0 ] || die "节点 ID 必须大于 0"
    [ -n "$API_KEY_ARG" ] || die "节点通讯密钥不能为空"
}

service_active() {
    [ "$SKIP_SERVICE" = "1" ] && return 1
    if [ "$RELEASE" = "alpine" ]; then
        service "$SERVICE_NAME" status >/dev/null 2>&1
    else
        systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null
    fi
}

stop_service() {
    [ "$SKIP_SERVICE" = "1" ] && return 0
    if [ "$RELEASE" = "alpine" ]; then
        service "$SERVICE_NAME" stop >/dev/null 2>&1 || true
    else
        systemctl stop "$SERVICE_NAME" >/dev/null 2>&1 || true
    fi
}

install_systemd_service() {
    local unit="$SYSTEMD_DIR/$SERVICE_NAME.service"
    mkdir -p "$SYSTEMD_DIR"
    if [ -f "$unit" ]; then
        cp -a "$unit" "$unit.bak-$(date +%Y%m%d-%H%M%S)" 2>/dev/null || true
    fi
    cat > "$unit" <<EOF
[Unit]
Description=v2node-rust Service
After=network-online.target nss-lookup.target
Wants=network-online.target

[Service]
User=root
Group=root
Type=simple
WorkingDirectory=$INSTALL_DIR
ExecStart=$BIN --config $CONFIG_FILE
Restart=always
RestartSec=10
LimitNOFILE=1048576
LimitCORE=infinity
TasksMax=65536
LimitNPROC=65536

[Install]
WantedBy=multi-user.target
EOF
    systemctl daemon-reload
}

install_openrc_service() {
    local unit="$OPENRC_DIR/$SERVICE_NAME"
    mkdir -p "$OPENRC_DIR"
    if [ -f "$unit" ]; then
        cp -a "$unit" "$unit.bak-$(date +%Y%m%d-%H%M%S)" 2>/dev/null || true
    fi
    cat > "$unit" <<EOF
#!/sbin/openrc-run

name="$SERVICE_NAME"
description="v2node-rust"
command="$BIN"
command_args="--config $CONFIG_FILE"
command_user="root"
pidfile="/run/$SERVICE_NAME.pid"
command_background="yes"
output_log="/var/log/$SERVICE_NAME.log"
error_log="/var/log/$SERVICE_NAME.log"

depend() {
    need net
}
EOF
    chmod +x "$unit"
}

install_service() {
    if [ "$SKIP_SERVICE" = "1" ]; then
        log_skip "已跳过服务安装（--skip-service）"
        return 0
    fi
    if [ "$RELEASE" = "alpine" ]; then
        install_openrc_service
        rc-update add "$SERVICE_NAME" default >/dev/null 2>&1 || true
        service "$SERVICE_NAME" restart >/dev/null 2>&1 || service "$SERVICE_NAME" start >/dev/null 2>&1 || true
    else
        install_systemd_service
        systemctl enable "$SERVICE_NAME" >/dev/null 2>&1 || true
        systemctl restart "$SERVICE_NAME" >/dev/null 2>&1 || systemctl start "$SERVICE_NAME" >/dev/null 2>&1 || true
    fi
    sleep 2
    if service_active; then
        echo -e "${green}$SERVICE_NAME 已启动${plain}"
    else
        warn "$SERVICE_NAME 可能启动失败，请使用 v2node-rust log 查看日志"
    fi
}

log_skip() {
    echo -e "${yellow}$*${plain}"
}

install_manager() {
    local source=""
    local self_dir
    self_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd || true)
    if [ -n "${V2NODE_RUST_MANAGER_SOURCE:-}" ] && [ -f "${V2NODE_RUST_MANAGER_SOURCE}" ]; then
        source=${V2NODE_RUST_MANAGER_SOURCE}
    elif [ -n "$self_dir" ] && [ -f "$self_dir/v2node.sh" ]; then
        source="$self_dir/v2node.sh"
    else
        local tmp
        tmp=$(mktemp) || return 1
        if download "https://raw.githubusercontent.com/$REPO/$BRANCH/script/v2node.sh" "$tmp"; then
            source=$tmp
        else
            rm -f "$tmp"
            warn "管理脚本下载失败，可稍后手动安装 script/v2node.sh"
            return 1
        fi
    fi
    mkdir -p "$(dirname "$MANAGER_PATH")" || return 1
    local temp="$MANAGER_PATH.new.$$"
    cp -f "$source" "$temp" && chmod 755 "$temp" && mv -f "$temp" "$MANAGER_PATH"
}

VERSION_ARG=""
LOCAL_PATH=""
URL_ARG=""
API_HOST_ARG=""
NODE_ID_ARG=""
API_KEY_ARG=""

while [ $# -gt 0 ]; do
    case "$1" in
        --local) LOCAL_PATH=${2:-}; shift 2 ;;
        --url) URL_ARG=${2:-}; shift 2 ;;
        --version) VERSION_ARG=${2:-}; shift 2 ;;
        --api-host) API_HOST_ARG=${2:-}; shift 2 ;;
        --node-id) NODE_ID_ARG=${2:-}; shift 2 ;;
        --api-key) API_KEY_ARG=${2:-}; shift 2 ;;
        --config) CONFIG_FILE=${2:-}; shift 2 ;;
        --skip-service) SKIP_SERVICE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        --*) die "未知参数：$1" ;;
        *)
            if [ -z "$VERSION_ARG" ]; then
                VERSION_ARG=$1
                shift
            else
                shift
            fi
            ;;
    esac
done

BIN="$INSTALL_DIR/v2node-rust"

[ "$(id -u)" -eq 0 ] || die "必须使用 root 用户运行此脚本"

if [ -n "$API_HOST_ARG" ] || [ -n "$NODE_ID_ARG" ] || [ -n "$API_KEY_ARG" ]; then
    validate_api_args
fi

if [ "$SKIP_SERVICE" != "1" ]; then
    if [ "$RELEASE" != "alpine" ] && systemctl is-active --quiet v2node 2>/dev/null; then
        warn "检测到 Go 版 v2node 正在运行；同一面板节点不要同时运行 Go 与 Rust 版本"
    fi
fi

ensure_deps

if [ -f "$BIN" ]; then
    warn "$BIN 已存在，将覆盖安装（旧二进制会备份为 .old-*）"
fi

TMP_DIR=$(mktemp -d) || die "无法创建临时目录"
trap 'rm -rf "$TMP_DIR"' EXIT

BIN_SRC=""
SRC_DIR=""
VERSION_LABEL=""

if [ -n "$LOCAL_PATH" ]; then
    if [ -d "$LOCAL_PATH" ]; then
        SRC_DIR=$LOCAL_PATH
        BIN_SRC=$SRC_DIR/v2node-rust
    else
        SRC_DIR=$(dirname "$LOCAL_PATH")
        BIN_SRC=$LOCAL_PATH
    fi
    [ -f "$BIN_SRC" ] || die "本地二进制不存在：$BIN_SRC"
    VERSION_LABEL="local"
elif [ -n "$URL_ARG" ]; then
    echo -e "${green}下载：${plain}$URL_ARG"
    download "$URL_ARG" "$TMP_DIR/asset" || die "下载失败：$URL_ARG"
    if tar -tzf "$TMP_DIR/asset" >/dev/null 2>&1; then
        mkdir -p "$TMP_DIR/pkg"
        tar -xzf "$TMP_DIR/asset" -C "$TMP_DIR/pkg" || die "解压失败"
        BIN_SRC=$(find "$TMP_DIR/pkg" -type f -name v2node-rust | head -n 1)
        [ -n "$BIN_SRC" ] || die "压缩包中未找到 v2node-rust"
        SRC_DIR=$(dirname "$BIN_SRC")
    else
        mkdir -p "$TMP_DIR/pkg"
        cp -f "$TMP_DIR/asset" "$TMP_DIR/pkg/v2node-rust"
        BIN_SRC="$TMP_DIR/pkg/v2node-rust"
        SRC_DIR="$TMP_DIR/pkg"
    fi
    VERSION_LABEL="custom"
else
    case "$(uname -m)" in
        x86_64|amd64|x64) ARCH=amd64 ;;
        aarch64|arm64) ARCH=arm64 ;;
        *) die "暂不支持 CPU 架构 $(uname -m)；请使用 --local 提供已编译二进制" ;;
    esac
    if [ -n "$VERSION_ARG" ]; then
        TAG="v${VERSION_ARG#v}"
    else
        TAG=$(fetch_text "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/' | head -n 1)
        [ -n "$TAG" ] || die "获取最新版本失败；请用 --version 指定版本，或设置 V2NODE_RUST_REPO"
    fi
    ASSET="v2node-rust-linux-${ARCH}.tar.gz"
    URL="https://github.com/$REPO/releases/download/$TAG/$ASSET"
    echo -e "${green}下载：${plain}$URL"
    download "$URL" "$TMP_DIR/asset" || die "下载失败；请确认版本、资产名和 V2NODE_RUST_REPO"
    mkdir -p "$TMP_DIR/pkg"
    if tar -tzf "$TMP_DIR/asset" >/dev/null 2>&1; then
        tar -xzf "$TMP_DIR/asset" -C "$TMP_DIR/pkg" || die "解压失败"
        BIN_SRC=$(find "$TMP_DIR/pkg" -type f -name v2node-rust | head -n 1)
        [ -n "$BIN_SRC" ] || die "压缩包中未找到 v2node-rust"
        SRC_DIR=$(dirname "$BIN_SRC")
    else
        cp -f "$TMP_DIR/asset" "$TMP_DIR/pkg/v2node-rust"
        BIN_SRC="$TMP_DIR/pkg/v2node-rust"
        SRC_DIR="$TMP_DIR/pkg"
    fi
    VERSION_LABEL=$TAG
fi

if ! "$BIN_SRC" --version >/dev/null 2>&1; then
    die "二进制无法在当前系统执行（架构不匹配或文件损坏）：$BIN_SRC"
fi
VERSION_TEXT=$("$BIN_SRC" --version 2>/dev/null | head -n 1)

if service_active; then
    echo "停止现有 $SERVICE_NAME ..."
    stop_service
fi

mkdir -p "$INSTALL_DIR" || die "无法创建安装目录 $INSTALL_DIR"
if [ -f "$BIN" ]; then
    cp -a "$BIN" "$BIN.old-$(date +%Y%m%d-%H%M%S)" 2>/dev/null || true
fi
cp -f "$BIN_SRC" "$BIN" || die "写入二进制失败"
chmod 755 "$BIN"

for helper in gen_x25519 vless_probe; do
    if [ -f "$SRC_DIR/$helper" ]; then
        cp -f "$SRC_DIR/$helper" "$INSTALL_DIR/$helper" 2>/dev/null || true
        chmod 755 "$INSTALL_DIR/$helper" 2>/dev/null || true
    fi
done

CONFIG_READY=0
if [ -f "$CONFIG_FILE" ]; then
    chmod 600 "$CONFIG_FILE" 2>/dev/null || true
    echo -e "${green}保留现有配置：${plain}$CONFIG_FILE"
    CONFIG_READY=1
elif [ -n "$API_HOST_ARG" ]; then
    generate_config "$CONFIG_FILE" "$API_HOST_ARG" "$NODE_ID_ARG" "$API_KEY_ARG"
    CONFIG_READY=1
elif [ -t 0 ]; then
    echo "首次安装需要生成配置："
    read -rp "面板 API 地址 [默认 https://example.com/]: " host
    host=${host:-https://example.com/}
    read -rp "节点 ID [默认 1]: " node_id
    node_id=${node_id:-1}
    read -rsp "节点通讯密钥: " api_key
    echo
    API_HOST_ARG=$host
    NODE_ID_ARG=$node_id
    API_KEY_ARG=$api_key
    validate_api_args
    generate_config "$CONFIG_FILE" "$API_HOST_ARG" "$NODE_ID_ARG" "$API_KEY_ARG"
    CONFIG_READY=1
else
    warn "未找到配置且无法交互；请用 --api-host/--node-id/--api-key 重新运行或手动创建 $CONFIG_FILE"
fi

if [ "$CONFIG_READY" = "1" ]; then
    install_service
else
    warn "配置未就绪，未启用/启动服务；请重新运行安装并传入 API 参数，或稍后用 v2node-rust generate 生成"
    if [ "$RELEASE" = "alpine" ]; then
        install_openrc_service 2>/dev/null || true
    elif [ "$RELEASE" != "unknown" ]; then
        install_systemd_service 2>/dev/null || true
    fi
fi

install_manager || true

echo "------------------------------------------"
echo -e "v2node-rust 安装完成：${green}${VERSION_TEXT}${plain} (${VERSION_LABEL})"
echo "管理命令：v2node-rust"
echo "  配置：$CONFIG_FILE"
echo "  服务：$SERVICE_NAME"
echo "  管理脚本：$MANAGER_PATH"
echo "------------------------------------------"
