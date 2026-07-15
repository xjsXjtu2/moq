#!/usr/bin/env bash
# 启动"抓包专用"Chrome:导出 TLS 密钥 + 对目标主机强制走 QUIC + 放行自签证书。
# 支持两个目标(第 1 个参数):
#   local (默认) -> 连本机 localhost:4443       (证书 SPKI 取自 spki.txt)
#   dev           -> 连开发机 8.161.228.205:4443 (证书 SPKI 取自 spki_dev.txt)
# 第 2 个参数可指定 Chrome profile 目录(首次用空 profile,复用用同一个)。
# 用法:
#   ./launch_chrome.sh                              # 连本机
#   ./launch_chrome.sh dev                          # 连开发机
#   ./launch_chrome.sh dev /tmp/wt_profile_first    # 连开发机并指定 profile
set -euo pipefail

WT_DIR="$HOME/wt-test"
KEYLOG="$HOME/sslkeylog.log"        # 与 Wireshark tls.keylog_file 一致 -> 打开抓包自动解密
PORT=4443
DEV_IP="8.161.228.205"

TARGET="${1:-local}"
PROFILE="${2:-/tmp/wt_profile_first}"

case "$TARGET" in
  local) HOST="localhost" ;;
  dev)   HOST="$DEV_IP" ;;
  *) echo "未知目标 '$TARGET'(可选: local | dev)"; exit 1 ;;
esac

# 同时放行本机与开发机两张证书的 SPKI(逗号分隔),切目标不用改放行列表
SPKI_LOCAL=$(cut -d= -f2- "$WT_DIR/spki.txt" 2>/dev/null || true)
SPKI_DEV=$(cut -d= -f2- "$WT_DIR/spki_dev.txt" 2>/dev/null || true)
SPKI_LIST=$(printf '%s\n%s\n' "$SPKI_LOCAL" "$SPKI_DEV" | awk 'NF' | paste -sd, -)

CHROME="/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"

echo "目标          = $TARGET ($HOST:$PORT)"
echo "SSLKEYLOGFILE = $KEYLOG"
echo "user-data-dir = $PROFILE"
echo "SPKI 放行列表 = $SPKI_LIST"
echo "打开页面后访问: https://$HOST:$PORT/  (会自动加载测试页)"

SSLKEYLOGFILE="$KEYLOG" "$CHROME" \
  --user-data-dir="$PROFILE" \
  --origin-to-force-quic-on="$HOST:$PORT" \
  --ignore-certificate-errors-spki-list="$SPKI_LIST" \
  --no-first-run --no-default-browser-check \
  "https://$HOST:$PORT/" \
  >/tmp/wt_chrome.log 2>&1 &

echo "Chrome 已启动 (PID $!)。日志: /tmp/wt_chrome.log"
