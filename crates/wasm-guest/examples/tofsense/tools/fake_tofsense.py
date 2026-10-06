#!/usr/bin/env python3
"""Simulate a TOFSense on TCP, the way ser2net/socat expose a real UART.

  port 7000: active output mode, NLink_TOFSense_Frame0 at 10 Hz, split into random chunks
  port 7001: query mode, answers NLink_TOFSense_Read_Frame0 (57 10 FF FF id FF FF sum)

    python3 fake_tofsense.py [--active-port 7000] [--query-port 7001]

Distance sweeps 0.20–3.00 m; about every 25th frame is "out of range" (status 1, signal 0).
"""
import argparse
import asyncio
import math
import random
import time

START = time.monotonic()


def checksum(data: bytes) -> int:
    return sum(data) & 0xFF


def frame(sensor_id: int, tick: int) -> bytes:
    distance_mm = int(1600 + 1400 * math.sin(tick / 20))
    status, signal = (1, 0) if tick % 25 == 24 else (0, 40 + tick % 30)
    elapsed = int((time.monotonic() - START) * 1000) & 0xFFFFFFFF
    body = bytes([0x57, 0x00, 0xFF, sensor_id])
    body += elapsed.to_bytes(4, "little")
    body += (distance_mm & 0xFFFFFF).to_bytes(3, "little")
    body += bytes([status]) + signal.to_bytes(2, "little") + bytes([0xFF])
    return body + bytes([checksum(body)])


async def active(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    tick = 0
    try:
        while True:
            data = frame(0, tick)
            tick += 1
            cut = random.randint(1, len(data) - 1) if tick % 3 == 0 else len(data)
            writer.write(data[:cut])
            await writer.drain()
            if cut < len(data):
                await asyncio.sleep(0.01)
                writer.write(data[cut:])
                await writer.drain()
            await asyncio.sleep(0.1)
    except (ConnectionError, asyncio.CancelledError):
        pass
    finally:
        writer.close()


async def query(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    tick = 0
    try:
        while True:
            request = await reader.readexactly(8)
            if request[:2] != b"\x57\x10" or checksum(request[:7]) != request[7]:
                continue
            writer.write(frame(request[4], tick))
            tick += 1
            await writer.drain()
    except (asyncio.IncompleteReadError, ConnectionError, asyncio.CancelledError):
        pass
    finally:
        writer.close()


async def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--active-port", type=int, default=7000)
    parser.add_argument("--query-port", type=int, default=7001)
    args = parser.parse_args()
    servers = [
        await asyncio.start_server(active, "127.0.0.1", args.active_port),
        await asyncio.start_server(query, "127.0.0.1", args.query_port),
    ]
    print(f"fake TOFSense: active on {args.active_port}, query on {args.query_port}", flush=True)
    await asyncio.gather(*(server.serve_forever() for server in servers))


if __name__ == "__main__":
    asyncio.run(main())
