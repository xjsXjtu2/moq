#!/usr/bin/env python3
"""
最小化 WebTransport over HTTP/3 echo 服务器 (aioquic 1.3.0)
- 接受 Extended CONNECT (:protocol=webtransport),回 200 建立会话
- echo 所有 WebTransport 双向流数据 与 数据报(datagram)
- 启用 0-RTT:内存保存 session ticket,供复用时做 early data
用法:
  python wt_echo_server.py            # 监听 0.0.0.0:4443
  python wt_echo_server.py --port 4443 --host 0.0.0.0 -v
  python wt_echo_server.py --host :: --no-0rtt    # 保留 PSK 恢复,禁用 0-RTT
  python wt_echo_server.py --host :: --no-resume  # 完全关闭票据,每次完整握手
"""
import argparse
import asyncio
import logging
import pathlib
from collections import defaultdict
from typing import Dict, Optional

from aioquic.asyncio import QuicConnectionProtocol, serve
from aioquic.h3.connection import H3_ALPN, H3Connection
from aioquic.h3.events import (
    DatagramReceived,
    H3Event,
    HeadersReceived,
    WebTransportStreamDataReceived,
)
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import ProtocolNegotiated, QuicEvent
from aioquic.tls import SessionTicket

logger = logging.getLogger("wt-echo")

# 测试页 HTML 从同目录的 index.html 读取(独立文件,便于阅读/编辑)。
# 首次读取后缓存到 _TEST_PAGE_CACHE。
_HERE = pathlib.Path(__file__).resolve().parent
_INDEX_HTML = _HERE / "index.html"
_TEST_PAGE_CACHE: Optional[bytes] = None


def get_test_page() -> bytes:
    global _TEST_PAGE_CACHE 
    _TEST_PAGE_CACHE = None  # 不用缓存，支持动态改写html
    if _TEST_PAGE_CACHE is None:
        try:
            _TEST_PAGE_CACHE = _INDEX_HTML.read_bytes()
        except OSError as exc:
            logger.error("读取测试页 %s 失败: %s;返回最小回退页", _INDEX_HTML, exc)
            _TEST_PAGE_CACHE = (
                b"<!doctype html><meta charset=utf-8>"
                b"<h2>index.html \344\270\215\345\255\230\345\234\250</h2>"
                b"<p>\350\257\267\347\241\256\350\256\244 index.html \344\270\216 "
                b"wt_echo_server.py \345\234\250\345\220\214\344\270\200\347\233\256\345\275\225\343\200\202</p>"
            )
    return _TEST_PAGE_CACHE


class WebTransportEcho:
    """单个 WebTransport 会话的 echo 逻辑。"""

    def __init__(self, session_id: int, http: H3Connection) -> None:
        self.session_id = session_id
        self.http = http

    def on_event(self, event: H3Event) -> None:
        # 数据报:回送 "s"+收到内容 (例:收到 "1" -> 回 "s1")
        if isinstance(event, DatagramReceived):
            reply = b"s" + event.data
            logger.info("会话 %d 收到 datagram %r -> 回 %r",
                        self.session_id, event.data, reply)
            self.http.send_datagram(self.session_id, reply)
        # 双向流数据:有数据才回送 "s"+内容；纯结束事件只关闭返回方向
        elif isinstance(event, WebTransportStreamDataReceived):
            if event.data:
                reply = b"s" + event.data
                logger.info(
                    "会话 %d 流 %d 收到 %r(ended=%s) -> 回 %r",
                    self.session_id, event.stream_id, event.data, event.stream_ended, reply,
                )
                self.http._quic.send_stream_data(
                    event.stream_id, reply, end_stream=event.stream_ended
                )
            elif event.stream_ended:
                logger.info(
                    "会话 %d 流 %d 收到结束事件(无数据) -> 关闭返回流",
                    self.session_id, event.stream_id,
                )
                self.http._quic.send_stream_data(
                    event.stream_id, b"", end_stream=True
                )


class WtServerProtocol(QuicConnectionProtocol):
    def __init__(self, *args, **kwargs) -> None:
        super().__init__(*args, **kwargs)
        self._http: Optional[H3Connection] = None
        self._sessions: Dict[int, WebTransportEcho] = {}

    def quic_event_received(self, event: QuicEvent) -> None:
        if isinstance(event, ProtocolNegotiated) and event.alpn_protocol in H3_ALPN:
            # enable_webtransport=True 打开 Extended CONNECT + datagram 支持
            self._http = H3Connection(self._quic, enable_webtransport=True)
        if self._http is not None:
            for h3_event in self._http.handle_event(event):
                self._on_h3_event(h3_event)

    def _on_h3_event(self, event: H3Event) -> None:
        if isinstance(event, HeadersReceived):
            headers = {k: v for k, v in event.headers}
            method = headers.get(b":method")
            protocol = headers.get(b":protocol")
            path = headers.get(b":path", b"")
            if method == b"CONNECT" and protocol == b"webtransport":
                # WT over H3的建联请求，本质是：H3的 Extended CONNECT
                # :method = CONNECT
                # :protocol = webtransport
                # :path = /wt
                logger.info("== Extended CONNECT 到 %r,接受 WebTransport 会话 (stream %d) ==",
                            path.decode(errors="replace"), event.stream_id)
                # 回 200 建立会话
                self._http.send_headers(
                    stream_id=event.stream_id,
                    headers=[(b":status", b"200")],
                )
                self._sessions[event.stream_id] = WebTransportEcho(event.stream_id, self._http)
                self.transmit()
            else:
                # 非 WebTransport 的普通 H3 请求:返回本地测试页
                logger.info("普通 H3 请求 %s %r -> 返回测试页", method, path.decode(errors="replace"))
                self._http.send_headers(event.stream_id, [(b":status", b"200"),
                                                          (b"content-type", b"text/html; charset=utf-8")])
                self._http.send_data(event.stream_id, get_test_page(), end_stream=True)
                self.transmit()
        elif isinstance(event, (DatagramReceived, WebTransportStreamDataReceived)):
            sid = event.session_id if isinstance(event, WebTransportStreamDataReceived) else event.stream_id
            handler = self._sessions.get(sid)
            if handler is not None:
                handler.on_event(event)
                self.transmit()


# ---- 0-RTT 支持:内存保存 session ticket ----
class TicketStore:
    def __init__(self, allow_early_data: bool = True) -> None:
        self.tickets: Dict[bytes, SessionTicket] = {}
        self.allow_early_data = allow_early_data

    def add(self, ticket: SessionTicket) -> None:
        if not self.allow_early_data:
            # 保留 PSK 会话恢复,但去掉 early_data 宣告 -> 禁止 0-RTT
            ticket.max_early_data_size = None
            logger.info("下发 session ticket(仅 1-RTT 恢复,已禁 0-RTT),label=%s",
                        ticket.ticket[:8].hex())
        else:
            logger.info("下发 session ticket(供 0-RTT/复用),label=%s", ticket.ticket[:8].hex())
        self.tickets[ticket.ticket] = ticket

    def pop(self, label: bytes) -> Optional[SessionTicket]:
        t = self.tickets.pop(label, None)
        logger.info("客户端出示 ticket -> %s", "命中(尝试 0-RTT)" if t else "未命中")
        return t


async def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("--host", default="0.0.0.0")
    p.add_argument("--port", type=int, default=4443)
    p.add_argument("--cert", default="cert.pem")
    p.add_argument("--key", default="key.pem")
    p.add_argument("-v", "--verbose", action="store_true")
    p.add_argument("--no-0rtt", action="store_true",
                   help="保留 PSK 会话恢复,但禁用 0-RTT(去掉 early_data 宣告)")
    p.add_argument("--no-resume", action="store_true",
                   help="完全关闭会话票据:不下发/不取回,每次都完整握手(隐含禁 0-RTT)")
    args = p.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        datefmt="%H:%M:%S",
    )

    config = QuicConfiguration(
        is_client=False,
        alpn_protocols=H3_ALPN,
        max_datagram_frame_size=65536,   # 打开 QUIC datagram(WebTransport datagram 依赖)
    )
    config.load_cert_chain(args.cert, args.key)

    store = TicketStore(allow_early_data=not args.no_0rtt)
    if args.no_resume:
        mode = "会话恢复=关闭, 0-RTT=关闭(每次完整握手)"
        ticket_handler = None
        ticket_fetcher = None
    elif args.no_0rtt:
        mode = "会话恢复=开启(PSK), 0-RTT=关闭"
        ticket_handler = store.add
        ticket_fetcher = store.pop
    else:
        mode = "会话恢复=开启(PSK), 0-RTT=开启"
        ticket_handler = store.add
        ticket_fetcher = store.pop
    logger.info("WebTransport echo 服务器监听 https://%s:%d [%s]", args.host, args.port, mode)
    await serve(
        args.host,
        args.port,
        configuration=config,
        create_protocol=WtServerProtocol,
        session_ticket_handler=ticket_handler,   # 握手末尾下发 ticket
        session_ticket_fetcher=ticket_fetcher,   # 复用时取回 ticket -> 触发 0-RTT
        retry=False,
    )
    await asyncio.Future()  # 常驻


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        pass
