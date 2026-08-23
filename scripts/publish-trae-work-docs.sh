#!/bin/sh
set -eu

# 这是一个单文件 Dufs 上传脚本。把本文件单独发送给朋友即可，不需要
# scripts 目录、docs 目录、.env 文件或其他配套文件。
#
# 朋友使用步骤：
#   1. 在终端进入本文件所在目录：
#        cd /脚本文件所在目录
#   2. 给脚本增加执行权限：
#        chmod +x publish-trae-work-docs.sh
#   3. 检查 SSH 和 Dufs 服务：
#        ./publish-trae-work-docs.sh --check
#   4. 上传生成的文档目录：
#        ./publish-trae-work-docs.sh ./dist "测试文件1"
#
# SSH 密码只在交互式提示中输入，不会保存到脚本、环境文件、命令参数或仓库。
# 每次执行只建立一个临时 SSH 连接，最多提示输入一次密码。
#
# 第二个参数是相对于 Dufs 管理根目录的目标路径，支持中文和多级目录，例如：
#   "测试文件1"
#   "项目文档/第一版"
# 目标目录会被本地产物目录整体替换。本地目录本身不会额外嵌套一层：
# ./dist/index.html 会变成 <目标目录>/index.html。
#
# 脚本使用 SSH 搭配 tar 上传，不使用 rsync，也不需要配置 SSH 别名。
# 上传时会排除常见敏感文件和 macOS 扩展属性。上传完成后，Dufs 会创建或复用
# 目标目录的 dir_password，并输出管理员地址和只读分享地址。
# Share URL 只能用于下载，匿名写操作会返回 HTTP 403。

# 以下固定配置让朋友只需要执行一条命令，并输入一次 SSH 密码。
SSH_HOST='mi6.gwghome.site'
SSH_USER='android'
PUBLIC_URL='http://mi6.gwghome.site:3000'
REMOTE_ROOT='/home/android/work/share'
PASSWORD_STORE='/home/android/.dufs-directory-auth.json'
LOCAL_URL='http://127.0.0.1:3000'

usage() {
    cat <<'EOF'
用法:
  ./publish-trae-work-docs.sh <output-dir> <directory-name>
  ./publish-trae-work-docs.sh --check

功能:
  将本地产物目录的内容上传到 Dufs 管理根目录下的目标目录。
  目标目录可以使用中文或多级路径，已有目标目录会被整体替换。
  上传成功后输出 Admin URL 和带 dir_password 的只读 Share URL。

参数:
  output-dir       本地产物目录，例如 ./dist、./site、./book
  directory-name   Dufs 管理根目录下的目标目录，支持中文

示例:
  ./publish-trae-work-docs.sh ./dist "测试文件1"
  ./publish-trae-work-docs.sh ./dist "项目文档/第一版"

首次使用:
  chmod +x publish-trae-work-docs.sh
  ./publish-trae-work-docs.sh --check
  # SSH 提示时输入远端账号密码

注意:
  不要上传源码、.env、令牌、私钥或内部配置。
  --check 只检查连接，不会上传文件。
EOF
}

fail() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

validate_destination() {
    destination=$1
    [ -n "$destination" ] || fail "directory-name is required"
    case "/$destination/" in
        */../* | */./*) fail "directory-name cannot contain . or .. path components" ;;
    esac
    case "$destination" in
        /* | */ | *[[:space:]]*)
            fail "directory-name contains an unsupported character"
            ;;
    esac
    for forbidden in "'" '`' '$' '\' ';' '&' '|' '<' '>' '(' ')' '?' '#' '%'; do
        case "$destination" in
            *"$forbidden"*) fail "directory-name contains an unsupported character" ;;
        esac
    done
}

require_command ssh
require_command tar
require_command find
require_command grep
require_command mktemp

case "${1:-}" in
    -h | --help)
        usage
        exit 0
        ;;
esac

control_dir=$(mktemp -d /tmp/dufs-tw.XXXXXX)
control_path=$control_dir/master
chmod 700 "$control_dir"

cleanup() {
    if [ -S "$control_path" ]; then
        ssh -6 \
            -o "ControlPath=$control_path" \
            -o ConnectTimeout=5 \
            -O exit \
            -l "$SSH_USER" "$SSH_HOST" >/dev/null 2>&1 || true
    fi
    rm -rf "$control_dir"
}
trap cleanup EXIT HUP INT TERM

ssh_check() {
    ssh -6 \
        -o ControlMaster=auto \
        -o ControlPersist=120 \
        -o "ControlPath=$control_path" \
        -o PreferredAuthentications=password,keyboard-interactive \
        -o PubkeyAuthentication=no \
        -o NumberOfPasswordPrompts=1 \
        -o ConnectTimeout=10 \
        -l "$SSH_USER" "$SSH_HOST" "$@"
}

check_environment() {
    printf 'Checking Dufs SSH connection.\n'
    printf 'SSH password may be requested once for this command.\n'
    ssh_check "
        set -e
        command -v python3 >/dev/null
        command -v curl >/dev/null
        command -v find >/dev/null
        mkdir -p '$REMOTE_ROOT'
        test -w '$REMOTE_ROOT'
        test -r '$PASSWORD_STORE'
        curl -fsS '$LOCAL_URL/__dufs__/health' >/dev/null
    " || fail "remote prerequisites failed; verify IPv6 connectivity and Dufs service"
    printf 'Environment check passed.\n'
}

if [ "${1:-}" = "--check" ]; then
    [ "$#" -eq 1 ] || fail "--check does not accept positional arguments"
    check_environment
    exit 0
fi

[ "$#" -eq 2 ] || {
    usage >&2
    exit 2
}

source_dir=${1%/}
destination=${2%/}

[ -d "$source_dir" ] || fail "output directory does not exist: $source_dir"
validate_destination "$destination"

remote_relative=$destination
remote_dir="$REMOTE_ROOT/$destination"

if find "$source_dir" -type l -print -quit | grep -q .; then
    fail "output directory contains symbolic links; materialize or remove them before publishing"
fi

check_environment
printf 'Publishing %s to %s\n' "$source_dir" "$remote_dir"

staging="${remote_dir}.uploading.$$"
ssh_check "rm -rf '$staging'; mkdir -p '$staging'"
COPYFILE_DISABLE=1
export COPYFILE_DISABLE
if ! tar -C "$source_dir" \
    --exclude='.DS_Store' \
    --exclude='.git' \
    --exclude='.env' \
    --exclude='.env.*' \
    --exclude='*.pem' \
    --exclude='id_rsa*' \
    --exclude='id_ed25519*' \
    --exclude='node_modules' \
    --no-xattrs \
    -cf - . |
    ssh_check "tar -xf - -C '$staging'"
then
    ssh_check "rm -rf '$staging'" || true
    fail "tar upload failed"
fi

ssh_check "
    set -e
    rm -rf '${remote_dir}.previous'
    if test -e '$remote_dir'; then mv '$remote_dir' '${remote_dir}.previous'; fi
    mv '$staging' '$remote_dir'
    rm -rf '${remote_dir}.previous'
    find '$remote_dir' -type d -exec chmod 775 {} +
    find '$remote_dir' -type f -exec chmod 664 {} +
    curl -sS -o /dev/null '$LOCAL_URL/$remote_relative/' || true
"

share_password=$(
    ssh_check \
        "python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))[\"directories\"][sys.argv[2]])' '$PASSWORD_STORE' '$remote_relative'"
)

[ -n "$share_password" ] || fail "the server did not return a directory password"

admin_url="${PUBLIC_URL%/}/$remote_relative/"
share_url="${PUBLIC_URL%/}/$remote_relative/?dir_password=$share_password"

printf '\nPublished successfully.\n'
printf 'Admin URL: %s\n' "$admin_url"
printf 'Share URL: %s\n' "$share_url"
