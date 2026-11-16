#!/usr/bin/env bash

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/env.sh"
# 发布包：构建 -> 复制二进制 -> 生成校验和与 VERSION.txt -> tar.gz -> 解压自校验。
#
#   bash tools/release.sh
#   bash tools/release.sh --target windows --out /tmp/dist
set -euo pipefail

die() {
  echo "release: $*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
用法: bash tools/release.sh [--target linux|windows] [--out DIR]

选项:
  --target linux|windows  发布目标，默认 linux
  --out DIR               输出目录，默认 <仓库根>/dist
  -h, --help              显示本帮助
EOF
}

TARGET=linux
OUT_DIR="$UI_ROOT/dist"

# 解析参数；--help 只打印用法，不碰构建和文件。
while [ "$#" -gt 0 ]; do
  case "$1" in
    --target)
      [ "$#" -ge 2 ] || die "--target 缺少参数"
      TARGET="$2"
      shift 2
      ;;
    --target=*)
      TARGET="${1#*=}"
      shift
      ;;
    --out)
      [ "$#" -ge 2 ] || die "--out 缺少参数"
      OUT_DIR="$2"
      shift 2
      ;;
    --out=*)
      OUT_DIR="${1#*=}"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      usage >&2
      die "未知参数: $1"
      ;;
  esac
done

[ -n "$OUT_DIR" ] || die "--out 不能为空"
case "$TARGET" in
  linux|windows) ;;
  *) die "不支持的 --target: $TARGET（可选 linux 或 windows）" ;;
esac

# 架构取自构建机；Windows 产物由 x86_64-pc-windows-gnu 工具链生成。
case "$(uname -m)" in
  x86_64|amd64) ARCH=x86_64 ;;
  aarch64|arm64) ARCH=aarch64 ;;
  *) ARCH="$(uname -m)" ;;
esac
[ "$TARGET" = windows ] && ARCH=x86_64

echo "== 构建 =="
# 按项目约定统一走 dev.sh，避免出现第二份构建产物。
bash "$UI_ROOT/tools/dev.sh" build --release

# 版本始终从 Linux 侧的 UI_BIN 读取：Windows 的 .exe 不保证能在 WSL 内直接执行。
[ -x "$UI_BIN" ] || die "找不到构建产物: $UI_BIN"
VERSION_LINE="$("$UI_BIN" --version 2>&1)" || die "读取版本失败: $VERSION_LINE"
VERSION_LINE="${VERSION_LINE%%$'\n'*}"

# 实际输出形如 "ui 0.1.0 (commit ...)"，第一段是程序名，版本号在其后。
read -r -a VERSION_FIELDS <<< "$VERSION_LINE"
VERSION="${VERSION_FIELDS[0]:-}"
[ "$VERSION" = ui ] && VERSION="${VERSION_FIELDS[1]:-}"
VERSION="${VERSION#v}"
VERSION="${VERSION#V}"
[[ "$VERSION" =~ ^[0-9][0-9A-Za-z._+-]*$ ]] || die "无法解析版本号: $VERSION_LINE"

# 取目标二进制：Linux 用 UI_BIN，Windows 用预编译的 UI_WIN_BIN。
if [ "$TARGET" = windows ]; then
  SRC_BIN="$UI_WIN_BIN"
  [ -f "$SRC_BIN" ] || die "找不到 Windows 产物: $SRC_BIN（请先运行 bash tools/build_windows.sh）"
else
  SRC_BIN="$UI_BIN"
  [ -f "$SRC_BIN" ] || die "找不到 Linux 产物: $SRC_BIN"
fi

PKG_NAME="ui-${VERSION}-${TARGET}-${ARCH}"
mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"
PKG_DIR="$OUT_DIR/$PKG_NAME"
ARCHIVE="$OUT_DIR/$PKG_NAME.tar.gz"

# 复用输出目录时先清掉同名旧目录，避免把历史文件打进新包。
rm -rf -- "$PKG_DIR"
mkdir -p "$PKG_DIR"
cp -- "$SRC_BIN" "$PKG_DIR/ui"
chmod 755 "$PKG_DIR/ui"

# 校验清单与二进制同目录，格式为 "<hash>  <文件名>"。
HASH="$(sha256sum "$PKG_DIR/ui" | awk '{print $1}')"
printf '%s  %s\n' "$HASH" ui > "$PKG_DIR/ui.sha256"

COMMIT="$(git -C "$UI_ROOT" rev-parse --short HEAD 2>/dev/null || true)"
COMMIT="${COMMIT:-unknown}"
BUILD_DATE="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
cat > "$PKG_DIR/VERSION.txt" <<EOF
名称: ui
版本: ${VERSION}
目标: ${TARGET}-${ARCH}
Commit: ${COMMIT}
构建日期: ${BUILD_DATE}
EOF

rm -f -- "$ARCHIVE"
tar -czf "$ARCHIVE" -C "$OUT_DIR" "$PKG_NAME"

# 自校验：重新解压，按包内清单重算 sha256，不一致立即非零退出。
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ui-release.XXXXXX")"
trap 'rm -rf "$TMP_DIR"' EXIT
tar -xzf "$ARCHIVE" -C "$TMP_DIR"
CHECK_DIR="$TMP_DIR/$PKG_NAME"

[ -f "$CHECK_DIR/ui" ] || die "自校验失败: 包内缺少 ui"
[ -x "$CHECK_DIR/ui" ] || die "自校验失败: 包内 ui 不可执行"
[ -f "$CHECK_DIR/ui.sha256" ] || die "自校验失败: 包内缺少 ui.sha256"
[ -s "$CHECK_DIR/VERSION.txt" ] || die "自校验失败: 包内缺少 VERSION.txt"

read -r WANT_HASH WANT_NAME < "$CHECK_DIR/ui.sha256" || die "自校验失败: 校验清单为空"
[ "$WANT_NAME" = ui ] || die "自校验失败: 清单文件名是 $WANT_NAME，应为 ui"
GOT_HASH="$(sha256sum "$CHECK_DIR/ui" | awk '{print $1}')"
[ "$WANT_HASH" = "$GOT_HASH" ] || die "自校验失败: sha256 不一致（清单 $WANT_HASH，实际 $GOT_HASH）"

echo
echo "== 发布完成 =="
echo "包: $ARCHIVE"
echo "目录: $PKG_DIR"
echo "sha256: $HASH  ui"
echo "自校验: 通过"
