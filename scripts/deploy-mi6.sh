#!/bin/sh
set -eu

# 将当前 Dufs 项目同步到 mi6，并在远端完成编译、安装、重启和健康检查。
#
# 同步源码，不编译或重启：
#   ./scripts/deploy-mi6.sh push
#
# 同步源码并完成编译、安装和重启：
#   ./scripts/deploy-mi6.sh install
#
# 只检查完整部署所需环境：
#   ./scripts/deploy-mi6.sh check
#
# 脚本不会保存 SSH 密码。若没有配置 SSH 密钥，每次执行最多提示一次密码。
# 可通过环境变量覆盖默认连接信息，详见 --help。

SSH_HOST=${DUFS_DEPLOY_HOST:-mi6.gwghome.site}
SSH_USER=${DUFS_DEPLOY_USER:-android}
SSH_PORT=${DUFS_DEPLOY_PORT:-22}
REMOTE_REPO=${DUFS_DEPLOY_REMOTE_REPO:-/home/android/work/dufs}
REMOTE_BINARY=${DUFS_DEPLOY_REMOTE_BINARY:-/usr/local/bin/dufs}
REMOTE_LOG=${DUFS_DEPLOY_REMOTE_LOG:-/home/android/dufs.log}
HEALTH_URL=${DUFS_DEPLOY_HEALTH_URL:-http://127.0.0.1:3000/__dufs__/health}

usage() {
    cat <<'EOF'
用法:
  ./scripts/deploy-mi6.sh push
  ./scripts/deploy-mi6.sh install
  ./scripts/deploy-mi6.sh check

完整部署流程:
  1. 检查本地命令和 mi6 远端环境
  2. 同步当前项目源码，保留远端 target/ 增量编译缓存
  3. 使用 CC=clang CXX=clang++ 执行 cargo build --release --locked
  4. 通过 sudo 原子更新 /usr/local/bin/dufs
  5. 继承当前 Dufs 进程参数进行重启
  6. 检查进程路径和 HTTP 健康状态
  7. 部署失败时恢复旧二进制并重新启动

模式:
  push      只同步源码，保留远端 target/，不编译、不安装、不重启
  install   同步源码，然后编译、安装并重启 Dufs
  check     检查完整部署所需环境，不同步代码

环境变量:
  DUFS_DEPLOY_HOST           SSH 主机
  DUFS_DEPLOY_USER           SSH 用户
  DUFS_DEPLOY_PORT           SSH 端口
  DUFS_DEPLOY_REMOTE_REPO    远端源码目录
  DUFS_DEPLOY_REMOTE_BINARY  安装目标
  DUFS_DEPLOY_REMOTE_LOG     服务日志
  DUFS_DEPLOY_HEALTH_URL     远端健康检查地址

说明:
  完整部署要求 SSH 用户可以免密执行 sudo，并且 Dufs 正在运行。
  管理员认证参数从当前 Dufs 进程继承，不会写入本脚本。
  push 不要求 sudo，也不要求 Dufs 正在运行。
EOF
}

fail() {
    printf '错误: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "缺少本地命令: $1"
}

validate_remote_value() {
    name=$1
    value=$2
    [ -n "$value" ] || fail "$name 不能为空"
    case "$value" in
        *"'"* | *'
'*) fail "$name 包含不支持的字符" ;;
    esac
}

case "${1:-}" in
    push)
        [ "$#" -eq 1 ] || fail "push 不接受其他参数"
        mode=push
        ;;
    install)
        [ "$#" -eq 1 ] || fail "install 不接受其他参数"
        mode=install
        ;;
    check)
        [ "$#" -eq 1 ] || fail "check 不接受其他参数"
        mode=check
        ;;
    -h | --help)
        usage
        exit 0
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac

require_command ssh
require_command tar
require_command mktemp

validate_remote_value DUFS_DEPLOY_HOST "$SSH_HOST"
validate_remote_value DUFS_DEPLOY_USER "$SSH_USER"
validate_remote_value DUFS_DEPLOY_PORT "$SSH_PORT"
validate_remote_value DUFS_DEPLOY_REMOTE_REPO "$REMOTE_REPO"
validate_remote_value DUFS_DEPLOY_REMOTE_BINARY "$REMOTE_BINARY"
validate_remote_value DUFS_DEPLOY_REMOTE_LOG "$REMOTE_LOG"
validate_remote_value DUFS_DEPLOY_HEALTH_URL "$HEALTH_URL"

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_root=$(dirname "$script_dir")
[ -f "$project_root/Cargo.toml" ] || fail "无法定位项目根目录"
[ -f "$project_root/Cargo.lock" ] || fail "项目缺少 Cargo.lock"
[ -d "$project_root/src" ] || fail "项目缺少 src 目录"

control_dir=$(mktemp -d "${TMPDIR:-/tmp}/dufs-deploy.XXXXXX")
control_path=$control_dir/master
remote_staging="${REMOTE_REPO}.sync.$$"
staging_created=0
chmod 700 "$control_dir"

ssh_run() {
    ssh -6 \
        -p "$SSH_PORT" \
        -o ControlMaster=auto \
        -o ControlPersist=120 \
        -o "ControlPath=$control_path" \
        -o PreferredAuthentications=publickey,password,keyboard-interactive \
        -o NumberOfPasswordPrompts=1 \
        -o ConnectTimeout=10 \
        -l "$SSH_USER" "$SSH_HOST" "$@"
}

cleanup() {
    if [ "$staging_created" -eq 1 ]; then
        ssh_run "rm -rf '$remote_staging'" >/dev/null 2>&1 || true
    fi
    if [ -S "$control_path" ]; then
        ssh -6 \
            -p "$SSH_PORT" \
            -o "ControlPath=$control_path" \
            -o ConnectTimeout=5 \
            -O exit \
            -l "$SSH_USER" "$SSH_HOST" >/dev/null 2>&1 || true
    fi
    rm -rf "$control_dir"
}
trap cleanup EXIT HUP INT TERM

check_sync_environment() {
    printf '检查 mi6 同步环境，SSH 密码最多提示一次。\n'
    ssh_run "
        set -eu
        for command in tar rsync; do
            command -v \"\$command\" >/dev/null
        done
        test -d '$REMOTE_REPO'
        test -w '$REMOTE_REPO'
    " || fail "远端同步环境检查失败，请检查网络、工具和源码目录权限"
    printf '同步环境检查通过。\n'
}

check_deploy_environment() {
    printf '检查 mi6 编译部署环境。\n'
    ssh_run "
        set -eu
        for command in clang clang++ curl python3 sudo pgrep readlink sha256sum; do
            command -v \"\$command\" >/dev/null
        done
        test -x \"\$HOME/.cargo/bin/cargo\"
        sudo -n true
        pid=\$(pgrep -u \"\$(id -u)\" -x dufs | head -n 1)
        test -n \"\$pid\"
        test -r \"/proc/\$pid/cmdline\"
        curl -fsS '$HEALTH_URL' >/dev/null
    " || fail "远端环境检查失败，请检查网络、工具、sudo 权限和 Dufs 服务"
    printf '编译部署环境检查通过。\n'
}

check_sync_environment
if [ "$mode" != push ]; then
    check_deploy_environment
fi
[ "$mode" = check ] && exit 0

printf '同步源码到 %s@%s:%s\n' "$SSH_USER" "$SSH_HOST" "$REMOTE_REPO"
ssh_run "rm -rf '$remote_staging'; mkdir -p '$remote_staging'"
staging_created=1

COPYFILE_DISABLE=1
export COPYFILE_DISABLE
if ! tar -C "$project_root" \
    --exclude='./.git' \
    --exclude='./.trae' \
    --exclude='./.vscode' \
    --exclude='./target' \
    --exclude='./.DS_Store' \
    --exclude='./.env' \
    --exclude='./.env.*' \
    --no-xattrs \
    -cf - . |
    ssh_run "tar -xf - -C '$remote_staging'"
then
    fail "源码上传失败"
fi

ssh_run "
    set -eu
    rsync -a --delete \
        --exclude='/.git/' \
        --exclude='/target/' \
        '$remote_staging/' '$REMOTE_REPO/'
    rm -rf '$remote_staging'
"
staging_created=0

[ "$mode" = push ] && {
    printf '源码同步完成，未执行编译、安装或服务重启。\n'
    exit 0
}

printf '在 mi6 上执行 release 编译。\n'
ssh_run "
    set -eu
    cd '$REMOTE_REPO'
    CC=clang CXX=clang++ \"\$HOME/.cargo/bin/cargo\" build --release --locked
    test -x target/release/dufs
    target/release/dufs --version
"

printf '安装新二进制并重启 Dufs。\n'
ssh_run "sh -s -- '$REMOTE_REPO' '$REMOTE_BINARY' '$REMOTE_LOG' '$HEALTH_URL'" <<'REMOTE_SCRIPT'
set -eu

remote_repo=$1
remote_binary=$2
remote_log=$3
health_url=$4
new_binary="$remote_repo/target/release/dufs"
backup="${remote_binary}.deploy-backup"
state_dir=$(mktemp -d "${TMPDIR:-/tmp}/dufs-install.XXXXXX")
cmdline_file="$state_dir/cmdline"
deployment_finished=0
replacement_installed=0

find_dufs_pid() {
    pgrep -u "$(id -u)" -x dufs | head -n 1
}

stop_process() {
    pid=$1
    [ -n "$pid" ] || return 0
    if ! kill -0 "$pid" 2>/dev/null; then
        return 0
    fi
    kill "$pid"
    count=0
    while kill -0 "$pid" 2>/dev/null && [ "$count" -lt 20 ]; do
        sleep 1
        count=$((count + 1))
    done
    if kill -0 "$pid" 2>/dev/null; then
        kill -KILL "$pid"
    fi
}

start_from_saved_command() {
    python3 - "$cmdline_file" "$remote_binary" "$remote_log" <<'PY'
import os
import subprocess
import sys

cmdline_file, binary, log_file = sys.argv[1:]
raw = open(cmdline_file, "rb").read().rstrip(b"\0")
args = [os.fsdecode(value) for value in raw.split(b"\0") if value]
if not args:
    raise SystemExit("saved Dufs command is empty")
args[0] = binary
with open(log_file, "ab", buffering=0) as log:
    process = subprocess.Popen(
        args,
        stdin=subprocess.DEVNULL,
        stdout=log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
        close_fds=True,
    )
print(process.pid)
PY
}

wait_for_health() {
    pid=$1
    count=0
    while [ "$count" -lt 15 ]; do
        if kill -0 "$pid" 2>/dev/null &&
            [ "$(readlink "/proc/$pid/exe" 2>/dev/null || true)" = "$remote_binary" ] &&
            curl -fsS "$health_url" >/dev/null 2>&1
        then
            return 0
        fi
        sleep 1
        count=$((count + 1))
    done
    return 1
}

rollback() {
    status=$?
    trap - EXIT HUP INT TERM
    if [ "$deployment_finished" -ne 1 ] && [ "$replacement_installed" -eq 1 ]; then
        printf '部署失败，正在恢复旧二进制。\n' >&2
        failed_pid=$(find_dufs_pid || true)
        stop_process "$failed_pid"
        if sudo -n test -f "$backup"; then
            sudo -n install -o root -g root -m 755 "$backup" "$remote_binary"
            rollback_pid=$(start_from_saved_command || true)
            if [ -n "$rollback_pid" ]; then
                wait_for_health "$rollback_pid" || true
            fi
        fi
    fi
    sudo -n rm -f "$backup" >/dev/null 2>&1 || true
    rm -rf "$state_dir"
    exit "$status"
}
trap rollback EXIT HUP INT TERM

old_pid=$(find_dufs_pid || true)
[ -n "$old_pid" ] || {
    printf '找不到正在运行的 Dufs，无法安全继承启动参数。\n' >&2
    exit 1
}
cp "/proc/$old_pid/cmdline" "$cmdline_file"
chmod 600 "$cmdline_file"

sudo -n test -x "$remote_binary"
sudo -n cp -p "$remote_binary" "$backup"
replacement_installed=1
sudo -n install -o root -g root -m 755 "$new_binary" "$remote_binary"

stop_process "$old_pid"
new_pid=$(start_from_saved_command)
if ! wait_for_health "$new_pid"; then
    printf '新版本未通过健康检查，最近日志如下：\n' >&2
    tail -n 30 "$remote_log" >&2 || true
    exit 1
fi

deployment_finished=1
sudo -n rm -f "$backup"
rm -rf "$state_dir"
trap - EXIT HUP INT TERM

printf '部署成功。\n'
printf '进程 PID: %s\n' "$new_pid"
printf '运行文件: %s\n' "$(readlink "/proc/$new_pid/exe")"
printf '文件校验: '
sha256sum "$remote_binary"
printf '健康检查: 通过\n'
REMOTE_SCRIPT

printf 'mi6 部署完成。\n'
