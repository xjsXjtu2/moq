# WebTransport (HTTP/3) echo demo —— 开发机部署说明

本机是 `8.161.228.205` 开发机。这里长期运行一份最小 WebTransport over HTTP/3 echo 服务（aioquic），
供任意客户端连测。抓包/解密/对比 0-RTT 的完整流程在**本机（macOS 一侧）**的 README 里，此文只讲开发机上的部署与运维。

代码同步自 macOS 的 `~/wt-test/`（tar over ssh，不含 pcapng/keylog/私钥缓存等大文件）。

---

## 部署概况

- 代码目录：`/root/wt-demo/`
- Python：`python38`（`dnf install python38`，因为系统自带的是 3.6，aioquic 需要 3.8+）
- venv：`/root/wt-demo/venv`，内装 `aioquic 1.2.0`（Python 3.8 上的最高版本，功能等同 1.3.0）
- 证书：`/root/wt-demo/{cert.pem,key.pem}`，本地自签，SAN 含 `DNS:localhost,IP:127.0.0.1,IP:8.161.228.205`
- 服务：systemd unit `wt-echo.service`，监听 `0.0.0.0:4443/udp`
- 默认模式：会话恢复(PSK) + 0-RTT 都开启

---

## 运维命令

```bash
systemctl status wt-echo            # 状态
systemctl restart wt-echo           # 重启
systemctl stop wt-echo              # 停止
journalctl -u wt-echo -n 50 --no-pager   # 日志（含「监听 https://0.0.0.0:4443 [模式]」行）
ss -lnup | grep 4443                # 确认 UDP 4443 在监听

# 本地端到端自测（发 1,2,3 期望收 s1,s2,s3）
cd /root/wt-demo && ./venv/bin/python verify_client.py
# 期望输出：收到回显: ['s1', 's2', 's3'] / 验证 通过 ✔
```

---

## 切换 0-RTT 模式

echo 服务的 0-RTT 由命令行开关控制，无需改代码。修改 systemd unit 的 `ExecStart` 参数即可：

| 模式 | ExecStart 参数 | 效果 |
|---|---|---|
| 开启 0-RTT（默认） | `--host 0.0.0.0 --port 4443` | PSK 恢复 + 0-RTT 都开 |
| 禁 0-RTT / 留恢复 | `... --no-0rtt` | 仍可 PSK 恢复（省证书往返），但不发 early_data |
| 完全禁恢复 | `... --no-resume` | 不下发票据，每次完整 1-RTT 握手 |

改法：

```bash
vi /etc/systemd/system/wt-echo.service      # 编辑 ExecStart 行，追加 --no-0rtt 或 --no-resume
systemctl daemon-reload && systemctl restart wt-echo
journalctl -u wt-echo -n 5 --no-pager       # 确认日志里的模式已切换
```

---

## 外网可达性

- 主机防火墙：firewalld 未运行、iptables INPUT 策略 ACCEPT → **主机层已全放行**。
- **阿里云 ECS 安全组**：已放行**入方向 UDP 4443**，公网客户端可连 `8.161.228.205:4443`。

---

## 从零重建（万一 venv/证书损坏）

```bash
cd /root/wt-demo
dnf -y install python38 python38-pip          # 若未装
python3.8 -m venv venv
./venv/bin/pip install --upgrade pip
./venv/bin/pip install aioquic
# 重新自签证书
openssl req -x509 -newkey rsa:2048 -nodes -keyout key.pem -out cert.pem \
  -days 365 -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1,IP:8.161.228.205"
systemctl restart wt-echo
```

---

## 文件清单

| 文件 | 作用 |
|---|---|
| `wt_echo_server.py` | echo 服务端（Extended CONNECT → 200，回显 datagram/stream，前缀 `s`） |
| `index.html` | 浏览器测试页（datagram/stream 模式、`?auto=1` 自动连接） |
| `verify_client.py` | 最小 CLI 客户端，自测 echo 是否通 |
| `cert.pem` / `key.pem` | 自签证书 / 私钥 |
| `capture.sh` / `launch_chrome.sh` | 抓包辅助脚本（主要在 macOS 一侧用） |
