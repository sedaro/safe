#!/usr/bin/env python3
"""Replay SAFE metrics.bin data to an OpenTelemetry Collector over HTTP.

The SAFE writer appends OTLP ExportMetricsServiceRequest messages without
length framing. Their repeated resource_metrics fields are extracted and
repacked into bounded OTLP requests for replay.
"""

import argparse
import sys
import time
from urllib import error, request

try:
    from scripts.average_memory import MetricsFormatError, _fields
except ModuleNotFoundError:  # Direct execution as ./scripts/replay_metrics.py.
    from average_memory import MetricsFormatError, _fields


def _varint(value: int) -> bytes:
    encoded = bytearray()
    while value >= 128:
        encoded.append((value & 0x7F) | 0x80)
        value >>= 7
    encoded.append(value)
    return bytes(encoded)


def _message_field(number: int, message: bytes) -> bytes:
    return _varint(number << 3 | 2) + _varint(len(message)) + message


def resource_metrics(data: bytes) -> list[bytes]:
    """Extract ResourceMetrics messages from concatenated OTLP requests."""
    resources = []
    for number, wire_type, value in _fields(data):
        if number != 1:
            continue
        if wire_type != 2:
            raise MetricsFormatError("resource_metrics is not a message")
        resources.append(value)  # type: ignore[arg-type]
    return resources


def batches(resources: list[bytes], batch_size: int):
    if batch_size < 1:
        raise ValueError("batch size must be positive")
    for offset in range(0, len(resources), batch_size):
        yield b"".join(_message_field(1, resource) for resource in resources[offset : offset + batch_size])


def send(endpoint: str, payload: bytes, timeout: float) -> None:
    http_request = request.Request(
        endpoint,
        data=payload,
        headers={"Content-Type": "application/x-protobuf"},
        method="POST",
    )
    try:
        with request.urlopen(http_request, timeout=timeout) as response:
            response.read()
    except error.HTTPError as exc:
        body = exc.read().decode("utf-8", errors="replace")
        raise RuntimeError(f"collector returned HTTP {exc.code}: {body}") from exc
    except error.URLError as exc:
        raise RuntimeError(f"could not reach collector: {exc.reason}") from exc


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("metrics_bin", help="path to serialized metrics.bin")
    parser.add_argument(
        "--endpoint",
        default="http://localhost:4318/v1/metrics",
        help="OTLP HTTP metrics endpoint (default: %(default)s)",
    )
    parser.add_argument(
        "--batch-size",
        type=int,
        default=100,
        help="number of ResourceMetrics records per request (default: %(default)s)",
    )
    parser.add_argument(
        "--delay",
        type=float,
        default=0,
        help="seconds to wait between requests (default: %(default)s)",
    )
    parser.add_argument("--timeout", type=float, default=30, help="HTTP timeout in seconds")
    args = parser.parse_args(argv)

    try:
        with open(args.metrics_bin, "rb") as metrics_file:
            resources = resource_metrics(metrics_file.read())
        request_batches = list(batches(resources, args.batch_size))
        for index, payload in enumerate(request_batches, start=1):
            send(args.endpoint, payload, args.timeout)
            print(f"sent batch {index}/{len(request_batches)} ({len(payload)} bytes)")
            if args.delay and index != len(request_batches):
                time.sleep(args.delay)
    except (OSError, MetricsFormatError, ValueError, RuntimeError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    print(f"replayed {len(resources)} ResourceMetrics records")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
