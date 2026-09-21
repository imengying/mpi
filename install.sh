#!/bin/sh
# pi 安装脚本：从 GitHub Release 下载（默认最新版）并安装到本机。
#
#   sh install.sh                       # 最新版 → ~/.local/bin
#   sh install.sh v0.1.3                # 指定版本（不带 v 也行）
#   sh install.sh --dir /usr/local/bin  # 指定目录（或环境变量 PI_INSTALL_DIR）
#   curl -fsSL <raw>/install.sh | sh    # 一键安装
#
# 校验和不用单独的 .sha256 附件：GitHub API 对每个资产自带 sha256 digest
# （与 Release 页面展示的一致），脚本直接对它校验。只依赖 POSIX sh、
# curl 或 wget、tar。

set -eu

REPO='imengying/mpi'

die() { printf '错误：%s\n' "$*" >&2; exit 1; }
warn() { printf '警告：%s\n' "$*" >&2; }

usage() {
  cat <<'EOF'
用法：sh install.sh [版本] [--dir <目录>]

  版本       要安装的 Release（如 v0.1.3 或 0.1.3，latest 表示最新），默认最新
  --dir      安装目录，默认 ~/.local/bin（可用环境变量 PI_INSTALL_DIR 覆盖）
EOF
}

# ---------------------------------------------------------------- 参数

VERSION_ARG=''
DIR="${PI_INSTALL_DIR:-}"
[ -n "$DIR" ] || [ -n "${HOME:-}" ] \
  || die '需要 HOME 环境变量（或用 --dir / PI_INSTALL_DIR 指定目录）'
: "${DIR:=$HOME/.local/bin}"

while [ $# -gt 0 ]; do
  case "$1" in
    --dir)
      [ $# -ge 2 ] || die '--dir 需要一个目录参数'
      DIR=$2; shift 2 ;;
    -h|--help)
      usage; exit 0 ;;
    -*)
      die "未知参数：$1（--help 看用法）" ;;
    *)
      [ -z "$VERSION_ARG" ] || die "版本号只需要给一次：$1"
      VERSION_ARG=$1; shift ;;
  esac
done

have() { command -v "$1" >/dev/null 2>&1; }

# ---------------------------------------------------------------- 下载工具

if have curl; then
  fetch() { curl -fsSL "$1" -o "$2"; }
elif have wget; then
  fetch() { wget -qO "$2" "$1"; }
else
  die '下载需要 curl 或 wget，两者都没有'
fi

# ---------------------------------------------------------------- 平台

OS=$(uname -s)
ARCH=$(uname -m)
case "$OS" in
  Linux)  TRIPLE_OS=unknown-linux-gnu ;;
  Darwin) TRIPLE_OS=apple-darwin ;;
  *) die "不支持的系统：$OS（产物只有 Linux 和 macOS）" ;;
esac
case "$ARCH" in
  x86_64)        TRIPLE_ARCH=x86_64 ;;
  aarch64|arm64) TRIPLE_ARCH=aarch64 ;;
  *) die "不支持的架构：$ARCH（产物只有 x86_64 和 aarch64）" ;;
esac
TARGET="$TRIPLE_ARCH-$TRIPLE_OS"

# Linux 产物在 ubuntu-24.04 上构建，绑 runner 自带的 glibc；过低装上也是起不来，
# 不如在下载前说清楚。musl（Alpine 等）没有对应产物。
if [ "$OS" = Linux ] && have ldd; then
  LDD_LINE=$(ldd --version 2>&1 | head -n 1)
  case "$LDD_LINE" in
    *musl*) die 'musl 系统（如 Alpine），暂无对应产物' ;;
  esac
  GLIBC_VER=$(printf '%s\n' "$LDD_LINE" | awk '{print $NF}')
  case "$GLIBC_VER" in
    [0-9]*)
      # awk 退出码 0 = 版本过低（if 条件里的失败不会触发 set -e）
      if awk -v v="$GLIBC_VER" 'BEGIN { split(v, a, ".");
          exit !(a[1]+0 < 2 || (a[1]+0 == 2 && a[2]+0 < 39)) }'; then
        die "glibc $GLIBC_VER 过低，Linux 产物需要 2.39+"
      fi
      ;;
    *)
      warn "从 ldd 输出认不出 glibc 版本（$LDD_LINE），跳过检查"
      ;;
  esac
elif [ "$OS" = Linux ]; then
  warn '找不到 ldd，跳过 glibc 版本检查'
fi

# ---------------------------------------------------------------- Release

if [ -z "$VERSION_ARG" ] || [ "$VERSION_ARG" = latest ]; then
  TAG=''
  API_URL="https://api.github.com/repos/$REPO/releases/latest"
else
  case "$VERSION_ARG" in
    v*) TAG="$VERSION_ARG" ;;
    *)  TAG="v$VERSION_ARG" ;;
  esac
  API_URL="https://api.github.com/repos/$REPO/releases/tags/$TAG"
fi

TMP=$(mktemp -d) || die '创建临时目录失败'
trap 'rm -rf "$TMP"' EXIT HUP INT TERM

fetch "$API_URL" "$TMP/release.json" \
  || die "获取 Release 信息失败：$API_URL（--version 的版本号可能不存在，或 API 匿名限流 60 次/小时）"
TAG=$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$TMP/release.json" | head -n 1)
[ -n "$TAG" ] || die 'Release 信息里读不到 tag_name（可能是限流，稍后再试）'
VERSION=${TAG#v}
ASSET="pi-${VERSION}-${TARGET}.tar.gz"
URL="https://github.com/$REPO/releases/download/$TAG/$ASSET"

# JSON 压成单行后：先定位精确的资产名（带前后引号，避免误配 .sha256 附件），
# 再取其后第一个 "digest"。资产对象里 name 在 digest 之前、两者之间不会出现
# 其他资产的 digest，所以取到的必是本资产的。解析失败只是跳过校验，不影响安装
# （下载地址由 tag 拼出，不靠 JSON 里的 URL）。
DIGEST=$(tr -d '\n' < "$TMP/release.json" \
  | awk -v n="\"name\": \"${ASSET}\"" '
      index($0, n) {
        rest = substr($0, index($0, n))
        if (match(rest, /"digest": *"sha256:[0-9a-fA-F]+/))
          print substr(rest, RSTART, RLENGTH)
      }' \
  | sed 's/.*sha256://' | head -n 1)

# ---------------------------------------------------------------- 下载安装

printf '==> 安装 pi %s（%s）\n' "$VERSION" "$TARGET"
fetch "$URL" "$TMP/$ASSET" || die "下载失败：$URL（该 Release 可能没有 $TARGET 产物）"

if [ -n "$DIGEST" ]; then
  if have sha256sum; then
    SUM=$(sha256sum "$TMP/$ASSET" | awk '{print $1}')
  elif have shasum; then
    SUM=$(shasum -a 256 "$TMP/$ASSET" | awk '{print $1}')
  else
    SUM=''; warn '没有 sha256sum / shasum，跳过校验'
  fi
  if [ -n "$SUM" ]; then
    [ "$SUM" = "$DIGEST" ] || die "sha256 不匹配：期望 $DIGEST，实际 $SUM"
    printf '==> sha256 校验通过\n'
  fi
else
  warn 'Release API 未提供 digest，跳过校验'
fi

tar -xzf "$TMP/$ASSET" -C "$TMP" --strip-components=1
[ -f "$TMP/pi" ] || die '压缩包里没有 pi 二进制'

mkdir -p "$DIR" || die "创建目录失败：$DIR"
DIR_ABS=$(cd "$DIR" && pwd) || die "进不去目录：$DIR"
if have install; then
  install -m 755 "$TMP/pi" "$DIR_ABS/pi"
else
  cp "$TMP/pi" "$DIR_ABS/pi" && chmod 755 "$DIR_ABS/pi"
fi

INSTALLED=$("$DIR_ABS/pi" --version 2>/dev/null || true)
[ "$INSTALLED" = "pi $VERSION" ] \
  || warn "安装的二进制报告版本为「${INSTALLED:-空}」，预期 pi $VERSION"
printf '==> 已安装 %s/pi（%s）\n' "$DIR_ABS" "${INSTALLED:-版本未知}"

case ":$PATH:" in
  *":$DIR_ABS:"*) ;;
  *)
    printf '==> %s 不在 PATH 里，把它加进去，例如在 ~/.zshrc 写：\n' "$DIR_ABS"
    printf '    export PATH="%s:$PATH"\n' "$DIR_ABS"
    ;;
esac
