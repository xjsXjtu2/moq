#!/usr/bin/env bash
# 抓 WebTransport (QUIC/UDP 4443) 流量。用法:
#   ./capture.sh first     # 抓首次建联
#   ./capture.sh resume    # 抓会话复用/0-RTT
# Ctrl-C 结束抓包。抓完用 Wireshark 打开 pcapng(已配好 keylog 自动解密)。
set -euo pipefail
TAG="${1:-cap}"
OUT="$HOME/wt-test/wt_${TAG}.pcapng"
TSHARK="/Applications/Wireshark.app/Contents/MacOS/tshark"

# lo0 = 本地回环(localhost 走它);抓真实网卡换 en0
echo "抓包 -> $OUT  (接口 lo0, udp port 4443)。Ctrl-C 停止。"
"$TSHARK" -i lo0 -f "udp port 4443" -w "$OUT"
echo "已保存: $OUT"
