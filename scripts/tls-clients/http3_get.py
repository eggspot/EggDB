import asyncio
import sys

from aioquic.asyncio.client import connect
from aioquic.asyncio.protocol import QuicConnectionProtocol
from aioquic.h3.connection import H3_ALPN, H3Connection
from aioquic.h3.events import DataReceived, HeadersReceived
from aioquic.quic.configuration import QuicConfiguration


class Client(QuicConnectionProtocol):
    def __init__(self, *arguments, **keywords):
        super().__init__(*arguments, **keywords)
        self.http = H3Connection(self._quic)
        self.finished = asyncio.get_event_loop().create_future()
        self.status = b""
        self.body = b""

    def quic_event_received(self, event):
        for received in self.http.handle_event(event):
            if isinstance(received, HeadersReceived):
                self.status = dict(received.headers).get(b":status", b"")
            elif isinstance(received, DataReceived):
                self.body += received.data
            if getattr(received, "stream_ended", False) and not self.finished.done():
                self.finished.set_result(True)


async def get(host, port, authority, authority_file, path):
    configuration = QuicConfiguration(is_client=True, alpn_protocols=H3_ALPN, server_name=authority)
    configuration.load_verify_locations(authority_file)
    async with connect(host, port, configuration=configuration, create_protocol=Client) as client:
        stream = client._quic.get_next_available_stream_id()
        client.http.send_headers(
            stream,
            [
                (b":method", b"GET"),
                (b":scheme", b"https"),
                (b":authority", authority.encode()),
                (b":path", path.encode()),
            ],
            end_stream=True,
        )
        client.transmit()
        await asyncio.wait_for(client.finished, 10)
        print(client.status.decode(), len(client.body))


if __name__ == "__main__":
    host, port, authority, authority_file, path = sys.argv[1:6]
    asyncio.run(get(host, int(port), authority, authority_file, path))
