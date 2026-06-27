#!/bin/bash
# ============================================================
# MoQ Boy 部署打包脚本
# 用法: ./deploy.sh
# 输出: ./deploy/moq-boy-YYYYMMDD-HHMMSS/
#
# 包含:
#   - moq-boy 二进制 (release)
#   - Web 前端 (webrtc.html + index.html)
#   - ROM 示例
#   - 启动脚本
# ============================================================
set -euo pipefail

cd "$(dirname "$0")"
PROJECT_ROOT="$(cd ../.. && pwd)"
TIMESTAMP=$(date +%Y%m%d-%H%M%S)
DEPLOY_DIR="./deploy/moq-boy-${TIMESTAMP}"

echo "=== MoQ Boy 部署打包 ==="
echo "时间: ${TIMESTAMP}"
echo "输出: ${DEPLOY_DIR}"
echo ""

# 1. 编译 release 二进制
echo "[1/4] 编译 moq-boy (release)..."
cd "$PROJECT_ROOT"
CC=clang cargo build --release --bin moq-boy --features webrtc
strip "$PROJECT_ROOT/target/release/moq-boy"
echo "  → target/release/moq-boy (stripped)"

# 2. 构建 Web 前端
echo "[2/4] 构建 Web 前端..."
cd "$PROJECT_ROOT/demo/boy"
bun --bun run vite build
echo "  → src/dist/"

# 3. 创建部署目录并拷贝文件
echo "[3/4] 拷贝文件..."
mkdir -p "$DEPLOY_DIR/bin"
mkdir -p "$DEPLOY_DIR/web"
mkdir -p "$DEPLOY_DIR/rom"

# 二进制
cp "$PROJECT_ROOT/target/release/moq-boy" "$DEPLOY_DIR/bin/moq-boy"
chmod +x "$DEPLOY_DIR/bin/moq-boy"

# Web 前端
cp src/dist/index.html "$DEPLOY_DIR/web/"
cp src/dist/webrtc.html "$DEPLOY_DIR/web/"
cp -r src/dist/assets "$DEPLOY_DIR/web/"

# ROM (如果有的话)
if [ -d rom ] && [ "$(ls -A rom 2>/dev/null)" ]; then
    cp rom/*.gb rom/*.gbc 2>/dev/null "$DEPLOY_DIR/rom/" || true
fi

echo "  → bin/moq-boy"
echo "  → web/"

# 4. 生成启动脚本
echo "[4/4] 生成启动脚本..."
cat > "$DEPLOY_DIR/start.sh" << 'STARTSCRIPT'
#!/bin/bash
# MoQ Boy 启动脚本
# 用法:
#   纯 WebRTC:   ./start.sh webrtc
#   纯 MoQ:      ./start.sh moq
#   混合模式:    ./start.sh hybrid
#   自定义:      ./start.sh <额外参数>

MODE="${1:-webrtc}"
BIND_IP="${BIND_IP:-0.0.0.0}"
WEBRTC_PORT="${WEBRTC_PORT:-8080}"
MOQ_PORT="${MOQ_PORT:-4443}"
UDP_ADDR="${UDP_ADDR:-}"
UDP_PORT="${UDP_PORT:-9000}"
ROM="${ROM:-rom/pokemon.gb}"
TLS_DOMAIN="${TLS_DOMAIN:-localhost}"

cd "$(dirname "$0")"

if [ -z "$UDP_ADDR" ]; then
    # 自动检测出口 IP
    UDP_ADDR=$(curl -s ifconfig.me 2>/dev/null || curl -s ip.sb 2>/dev/null || echo "")
    if [ -z "$UDP_ADDR" ]; then
        echo "WARNING: 无法自动检测出口 IP，请设置 UDP_ADDR 环境变量"
        echo "  export UDP_ADDR=<你的公网IP>"
    fi
fi

ARGS=(
    --rom "$ROM"
    --tls-generate "$TLS_DOMAIN"
    --log-level info
)

case "$MODE" in
    webrtc)
        echo "=== 启动 WebRTC 直连模式 ==="
        ARGS+=(
            --webrtc-listen "${BIND_IP}:${WEBRTC_PORT}"
            --webrtc-udp-addr "$UDP_ADDR"
            --webrtc-udp-port "$UDP_PORT"
        )
        ;;
    moq)
        echo "=== 启动 MoQ 直连模式 ==="
        ARGS+=(
            --moq-listen "${BIND_IP}:${MOQ_PORT}"
        )
        ;;
    hybrid)
        echo "=== 启动混合模式 (WebRTC + MoQ) ==="
        ARGS+=(
            --webrtc-listen "${BIND_IP}:${WEBRTC_PORT}"
            --webrtc-udp-addr "$UDP_ADDR"
            --webrtc-udp-port "$UDP_PORT"
            --moq-listen "${BIND_IP}:${MOQ_PORT}"
        )
        ;;
    *)
        ARGS+=($@)
        ;;
esac

echo ""
echo "启动命令: ./bin/moq-boy ${ARGS[*]}"
echo ""
exec ./bin/moq-boy "${ARGS[@]}"
STARTSCRIPT
chmod +x "$DEPLOY_DIR/start.sh"

# 5. 输出信息
echo ""
echo "=== 打包完成 ==="
echo "目录: $DEPLOY_DIR"
echo ""
echo "文件列表:"
find "$DEPLOY_DIR" -type f | sed "s|$DEPLOY_DIR/||" | sort
echo ""
echo "部署步骤:"
echo "  1. 上传: scp -r $DEPLOY_DIR user@your-server:/opt/moq-boy/"
echo "  2. 启动 WebRTC:  ssh user@your-server 'cd /opt/moq-boy && UDP_ADDR=1.2.3.4 ./start.sh webrtc'"
echo "  3. 启动混合:    ssh user@your-server 'cd /opt/moq-boy && UDP_ADDR=1.2.3.4 ./start.sh hybrid'"
echo "  4. 访问 WebRTC: https://your-server:5173/webrtc.html?url=https://your-server:WEBRTC_PORT"
echo "  5. 访问 MoQ:    https://your-server:5173/?url=https://your-server:MOQ_PORT/anon"
