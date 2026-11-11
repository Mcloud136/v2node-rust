#!/bin/bash
# v2node-rust manager
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
v2node-rust 管理脚本

用法:
  v2node-rust                 显示管理菜单
  v2node-rust install [参数]  安装（参数透传给 install.sh）
  v2node-rust update [版本]   更新到最新版或指定版本
  v2node-rust uninstall       卸载
  v2node-rust start|stop|restart|status
  v2node-rust enable|disable  开机自启
  v2node-rust log             查看日志
  v2node-rust config          编辑配置并重启
  v2node-rust generate        交互式生成配置
  v2node-rust x25519          生成 x25519 密钥（需 gen_x25519）
  v2node-rust version         查看版本
  v2node-rust update_shell    更新本管理脚本
  v2node-rust help            显示帮助

环境变量:
  V2NODE_RUST_REPO            GitHub 仓库（owner/repo）
  V2NODE_RUST_BRANCH          分支（默认 main）
  V2NODE_RUST_INSTALL_DIR     安装目录（默认 $INSTALL_DIR）
  V2NODE_RUST_CONFIG          配置文件（默认 $CONFIG_FILE）
  V2NODE_RUST_SERVICE         服务名（默认 $SERVICE_NAME）
  V2NODE_RUST_INSTALLER       指定本地 install.sh
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

confirm() {
    local prompt=$1
    local default=${2:-n}
    local answer
    read -rp "$prompt [默认$default]: " answer
    answer=${answer:-$default}
    case "$answer" in
        y|Y|yes|YES) return 0 ;;
        *) return 1 ;;
    esac
}

download() {
    local url=$1
    local output=$2
    if command -v curl >/dev/null 2>&1; then
        curl -fLsS --connect-timeout 10 --retry 3 --retry-delay 1 -o "$output" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget -q --tries=3 --timeout=10 -O "$output" "$url"
    else
        return 1
    fi
}

check_status() {
    if [ ! -x "$BIN" ]; then
        return 2
    fi
    if [ "$SKIP_SERVICE" = "1" ]; then
        return 1
    fi
    if [ "$RELEASE" = "alpine" ]; then
        service "$SERVICE_NAME" status >/dev/null 2>&1
    else
        systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null
    fi
}

check_enabled() {
    if [ "$RELEASE" = "alpine" ]; then
        rc-update show 2>/dev/null | grep -q "[[:space:]]$SERVICE_NAME[[:space:]]"
    else
        systemctl is-enabled --quiet "$SERVICE_NAME" 2>/dev/null
    fi
}

show_status() {
    check_status
    case $? in
        0) echo -e "v2node-rust 状态：${green}已运行${plain}" ;;
        1) echo -e "v2node-rust 状态：${yellow}未运行${plain}" ;;
        2) echo -e "v2node-rust 状态：${red}未安装${plain}"; return ;;
    esac
    if check_enabled; then
        echo -e "开机自启：${green}是${plain}"
    else
        echo -e "开机自启：${red}否${plain}"
    fi
    echo "版本：$("$BIN" --version 2>/dev/null | head -n 1)"
    echo "配置：$CONFIG_FILE"
}

service_start() {
    if [ "$SKIP_SERVICE" = "1" ]; then echo "已跳过服务操作"; return 0; fi
    if [ "$RELEASE" = "alpine" ]; then
        service "$SERVICE_NAME" start
    else
        systemctl start "$SERVICE_NAME"
    fi
}

service_stop() {
    if [ "$SKIP_SERVICE" = "1" ]; then echo "已跳过服务操作"; return 0; fi
    if [ "$RELEASE" = "alpine" ]; then
        service "$SERVICE_NAME" stop
    else
        systemctl stop "$SERVICE_NAME"
    fi
}

service_restart() {
    if [ "$SKIP_SERVICE" = "1" ]; then echo "已跳过服务操作"; return 0; fi
    if [ "$RELEASE" = "alpine" ]; then
        service "$SERVICE_NAME" restart
    else
        systemctl restart "$SERVICE_NAME"
    fi
}

service_enable() {
    if [ "$SKIP_SERVICE" = "1" ]; then echo "已跳过服务操作"; return 0; fi
    if [ "$RELEASE" = "alpine" ]; then
        rc-update add "$SERVICE_NAME" default
    else
        systemctl enable "$SERVICE_NAME"
    fi
}

service_disable() {
    if [ "$SKIP_SERVICE" = "1" ]; then echo "已跳过服务操作"; return 0; fi
    if [ "$RELEASE" = "alpine" ]; then
        rc-update del "$SERVICE_NAME" default
    else
        systemctl disable "$SERVICE_NAME"
    fi
}

show_log() {
    if [ "$RELEASE" = "alpine" ]; then
        if [ -f "/var/log/$SERVICE_NAME.log" ]; then
            tail -n 200 -f "/var/log/$SERVICE_NAME.log"
        else
            echo "未找到 /var/log/$SERVICE_NAME.log"
        fi
    else
        journalctl -u "$SERVICE_NAME.service" -e --no-pager -f
    fi
}

generate_config() {
    local api_host=$1
    local node_id=$2
    local api_key=$3
    mkdir -p "$(dirname "$CONFIG_FILE")" || die "无法创建配置目录"
    local tmp
    tmp=$(mktemp "${CONFIG_FILE}.XXXXXX") || die "无法创建临时配置文件"
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
            "ApiHost": "$(printf '%s' "$api_host" | sed 's/\\/\\\\/g; s/"/\\"/g')",
            "NodeID": ${node_id},
            "ApiKey": "$(printf '%s' "$api_key" | sed 's/\\/\\\\/g; s/"/\\"/g')",
            "Timeout": 15
        }
    ]
}
EOF
    chmod 600 "$tmp" 2>/dev/null || true
    mv -f "$tmp" "$CONFIG_FILE"
    echo -e "${green}配置已生成：${plain}$CONFIG_FILE"
}

validate_config_args() {
    [ -n "$1" ] && [ -n "$2" ] && [ -n "$3" ] || die "API 地址、节点 ID、通讯密钥不能为空"
    case "$1" in
        http://*|https://*) ;;
        *) die "面板 API 地址必须以 http:// 或 https:// 开头" ;;
    esac
    case "$2" in
        ''|*[!0-9]*) die "节点 ID 必须是数字" ;;
    esac
    [ "$2" -gt 0 ] || die "节点 ID 必须大于 0"
}

generate_config_file() {
    local host node_id api_key
    read -rp "面板 API 地址 [默认 https://example.com/]: " host
    host=${host:-https://example.com/}
    read -rp "节点 ID [默认 1]: " node_id
    node_id=${node_id:-1}
    read -rsp "节点通讯密钥: " api_key
    echo
    validate_config_args "$host" "$node_id" "$api_key"
    generate_config "$host" "$node_id" "$api_key"
    check_status
    if [ $? != 2 ]; then
        service_restart >/dev/null 2>&1 || true
        sleep 2
        if check_status; then
            echo -e "${green}v2node-rust 重启成功${plain}"
        else
            warn "v2node-rust 可能启动失败，请执行 v2node-rust log 查看日志"
        fi
    fi
}

run_installer() {
    local installer=${V2NODE_RUST_INSTALLER:-}
    local self_dir
    self_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" 2>/dev/null && pwd || true)
    if [ -z "$installer" ] && [ -n "$self_dir" ] && [ -f "$self_dir/install.sh" ]; then
        installer="$self_dir/install.sh"
    fi
    if [ -n "$installer" ]; then
        [ -f "$installer" ] || die "找不到安装脚本：$installer"
        bash "$installer" "$@"
        return $?
    fi
    local tmp
    tmp=$(mktemp) || die "无法创建临时文件"
    if ! download "https://raw.githubusercontent.com/$REPO/$BRANCH/script/install.sh" "$tmp"; then
        rm -f "$tmp"
        die "下载 install.sh 失败；请设置 V2NODE_RUST_REPO，或用 V2NODE_RUST_INSTALLER 指定本地 install.sh"
    fi
    bash "$tmp" "$@"
    local result=$?
    rm -f "$tmp"
    return $result
}

install_cmd() {
    check_status
    if [ $? != 2 ]; then
        echo -e "${yellow}v2node-rust 已安装；如需升级请使用 update${plain}"
        return 1
    fi
    run_installer --config "$CONFIG_FILE" "$@"
}

update_cmd() {
    local version=${1:-}
    check_status
    if [ $? = 2 ]; then
        echo -e "${red}请先安装 v2node-rust${plain}"
        return 1
    fi
    if [ -n "$version" ]; then
        run_installer --config "$CONFIG_FILE" --version "$version"
    else
        run_installer --config "$CONFIG_FILE"
    fi
    if [ $? = 0 ]; then
        echo -e "${green}更新完成${plain}"
    fi
}

uninstall_cmd() {
    check_status
    if [ $? = 2 ]; then
        echo -e "${red}v2node-rust 未安装${plain}"
        return 1
    fi
    confirm "确定要卸载 v2node-rust 吗？" "n" || return 0

    service_stop >/dev/null 2>&1 || true
    service_disable >/dev/null 2>&1 || true

    if [ "$RELEASE" = "alpine" ]; then
        rm -f "$OPENRC_DIR/$SERVICE_NAME"
    else
        rm -f "$SYSTEMD_DIR/$SERVICE_NAME.service"
        systemctl daemon-reload >/dev/null 2>&1 || true
        systemctl reset-failed "$SERVICE_NAME" >/dev/null 2>&1 || true
    fi

    rm -f "$BIN" "$BIN".old-* "$INSTALL_DIR/gen_x25519" "$INSTALL_DIR/vless_probe"

    if confirm "是否删除配置文件 $CONFIG_FILE？" "n"; then
        rm -f "$CONFIG_FILE"
    fi

    echo ""
    echo -e "${green}卸载完成${plain}"
    echo "管理脚本仍在 $MANAGER_PATH，可手动执行 rm -f $MANAGER_PATH 删除"
}

start_cmd() {
    check_status
    local status=$?
    if [ "$status" = 2 ]; then echo -e "${red}请先安装 v2node-rust${plain}"; return 1; fi
    if [ "$status" = 0 ]; then echo -e "${green}v2node-rust 已运行${plain}"; return 0; fi
    service_start
    sleep 2
    if check_status; then
        echo -e "${green}v2node-rust 启动成功，请使用 v2node-rust log 查看日志${plain}"
    else
        warn "v2node-rust 可能启动失败，请使用 v2node-rust log 查看日志"
    fi
}

stop_cmd() {
    check_status
    [ $? = 2 ] && { echo -e "${red}请先安装 v2node-rust${plain}"; return 1; }
    service_stop
    sleep 1
    if check_status; then
        warn "v2node-rust 仍在运行，请稍后检查日志"
    else
        echo -e "${green}v2node-rust 已停止${plain}"
    fi
}

restart_cmd() {
    check_status
    [ $? = 2 ] && { echo -e "${red}请先安装 v2node-rust${plain}"; return 1; }
    service_restart
    sleep 2
    if check_status; then
        echo -e "${green}v2node-rust 重启成功${plain}"
    else
        warn "v2node-rust 可能启动失败，请使用 v2node-rust log 查看日志"
    fi
}

status_cmd() {
    check_status
    if [ $? = 2 ]; then
        echo -e "${red}v2node-rust 未安装${plain}"
        return 1
    fi
    if [ "$SKIP_SERVICE" = "1" ]; then
        show_status
        return 0
    fi
    if [ "$RELEASE" = "alpine" ]; then
        service "$SERVICE_NAME" status || true
    else
        systemctl status "$SERVICE_NAME" --no-pager -l || true
    fi
}

enable_cmd() {
    check_status
    [ $? = 2 ] && { echo -e "${red}请先安装 v2node-rust${plain}"; return 1; }
    service_enable
    echo -e "${green}已设置开机自启${plain}"
}

disable_cmd() {
    check_status
    [ $? = 2 ] && { echo -e "${red}请先安装 v2node-rust${plain}"; return 1; }
    service_disable
    echo -e "${green}已取消开机自启${plain}"
}

config_cmd() {
    if [ ! -f "$CONFIG_FILE" ]; then
        generate_config_file
        return
    fi
    "${EDITOR:-vi}" "$CONFIG_FILE"
    chmod 600 "$CONFIG_FILE" 2>/dev/null || true
    echo "配置已修改，正在重启 v2node-rust ..."
    restart_cmd
}

version_cmd() {
    if [ ! -x "$BIN" ]; then
        echo -e "${red}v2node-rust 未安装${plain}"
        return 1
    fi
    "$BIN" --version
}

x25519_cmd() {
    if [ -x "$INSTALL_DIR/gen_x25519" ]; then
        "$INSTALL_DIR/gen_x25519"
    else
        echo -e "${red}未找到 $INSTALL_DIR/gen_x25519；请安装包含该工具的发行包${plain}"
    fi
}

update_shell() {
    local tmp
    tmp=$(mktemp) || die "无法创建临时文件"
    if ! download "https://raw.githubusercontent.com/$REPO/$BRANCH/script/v2node.sh" "$tmp"; then
        rm -f "$tmp"
        die "下载管理脚本失败；请确认 V2NODE_RUST_REPO / 网络"
    fi
    local target="$MANAGER_PATH.new.$$"
    cp -f "$tmp" "$target"
    chmod 755 "$target"
    mv -f "$target" "$MANAGER_PATH"
    rm -f "$tmp"
    echo -e "${green}管理脚本已更新，请重新运行${plain}"
}

show_menu() {
    while true; do
        echo -e "
  ${green}v2node-rust 管理脚本${plain}（不适用于 Docker）
--- ${REPO} ---
  ${green}0.${plain}  修改配置
————————————————
  ${green}1.${plain}  安装 v2node-rust
  ${green}2.${plain}  更新 v2node-rust
  ${green}3.${plain}  卸载 v2node-rust
————————————————
  ${green}4.${plain}  启动
  ${green}5.${plain}  停止
  ${green}6.${plain}  重启
  ${green}7.${plain}  查看状态
  ${green}8.${plain}  查看日志
————————————————
  ${green}9.${plain}  设置开机自启
  ${green}10.${plain} 取消开机自启
————————————————
  ${green}11.${plain} 查看版本
  ${green}12.${plain} 升级维护脚本
  ${green}13.${plain} 生成配置
  ${green}14.${plain} 生成 x25519 密钥
  ${green}15.${plain} 退出
"
        show_status
        echo
        read -rp "请输入选择 [0-15]: " num
        case "$num" in
            0) config_cmd ;;
            1) install_cmd ;;
            2) update_cmd ;;
            3) uninstall_cmd ;;
            4) start_cmd ;;
            5) stop_cmd ;;
            6) restart_cmd ;;
            7) status_cmd ;;
            8) show_log ;;
            9) enable_cmd ;;
            10) disable_cmd ;;
            11) version_cmd ;;
            12) update_shell ;;
            13) generate_config_file ;;
            14) x25519_cmd ;;
            15) exit 0 ;;
            *) echo -e "${red}请输入正确的数字 [0-15]${plain}" ;;
        esac
        echo
        read -rp "按回车返回主菜单..." _
        echo
    done
}

for arg in "$@"; do
    case "$arg" in
        -h|--help|help) usage; exit 0 ;;
    esac
done

[ "$(id -u)" -eq 0 ] || die "必须使用 root 用户运行此脚本"

if [ $# -eq 0 ]; then
    show_menu
    exit 0
fi

case "${1:-}" in
    install) shift; install_cmd "$@" ;;
    update) shift; update_cmd "${1:-}" ;;
    uninstall) uninstall_cmd ;;
    start) start_cmd ;;
    stop) stop_cmd ;;
    restart) restart_cmd ;;
    status) status_cmd ;;
    enable) enable_cmd ;;
    disable) disable_cmd ;;
    log) show_log ;;
    config) config_cmd ;;
    generate) generate_config_file ;;
    version) version_cmd ;;
    x25519) x25519_cmd ;;
    update_shell) update_shell ;;
    *) usage ;;
esac
