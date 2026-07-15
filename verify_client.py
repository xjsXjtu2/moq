#!/usr/bin/env python3
"""最小 WebTransport 客户端,验证服务端 s 前缀 echo。发 1,2,3 期望收 s1,s2,s3。"""
import asyncio, ssl
from aioquic.asyncio import connect
from aioquic.asyncio.protocol import QuicConnectionProtocol
from aioquic.h3.connection import H3_ALPN, H3Connection
from aioquic.h3.events import DatagramReceived, HeadersReceived
from aioquic.quic.configuration import QuicConfiguration
from aioquic.quic.events import QuicEvent

RESULTS = []

class Client(QuicConnectionProtocol):
    def __init__(self, *a, **k):
        super().__init__(*a, **k)
        self.http = H3Connection(self._quic, enable_webtransport=True)
        self.session_id = None
        self.connected = asyncio.get_event_loop().create_future()

    def quic_event_received(self, event: QuicEvent):
        for e in self.http.handle_event(event):
            if isinstance(e, HeadersReceived):
                status = dict(e.headers).get(b":status")
                if status == b"200" and not self.connected.done():
                    self.connected.set_result(True)
            elif isinstance(e, DatagramReceived):
                RESULTS.append(e.data.decode())

    async def run(self):
        self.session_id = self.http.create_webtransport_stream  # placeholder
        sid = self._quic.get_next_available_stream_id(is_unidirectional=False)
        self.http.send_headers(sid, [
            (b":method", b"CONNECT"), (b":protocol", b"webtransport"),
            (b":scheme", b"https"), (b":authority", b"localhost:4443"),
            (b":path", b"/wt"),
        ])
        self.session_id = sid
        self.transmit()
        await asyncio.wait_for(self.connected, timeout=5)
        for i in range(1, 4):
            self.http.send_datagram(self.session_id, str(i).encode())
            self.transmit()
            await asyncio.sleep(0.6)

async def main():
    cfg = QuicConfiguration(is_client=True, alpn_protocols=H3_ALPN,
                            max_datagram_frame_size=65536)
    cfg.verify_mode = ssl.CERT_NONE
    async with connect("localhost", 4443, configuration=cfg,
                       create_protocol=Client) as proto:
        await proto.run()
        await asyncio.sleep(1)
    print("收到回显:", RESULTS)
    ok = RESULTS[:3] == ["s1", "s2", "s3"]
    print("验证", "通过 ✔" if ok else "失败 �’")

asyncio.run(main())
