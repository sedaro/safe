#!/usr/bin/env python3
"""Report average sandbox.memory.usage from a SAFE metrics.bin file.

SAFE appends protobuf ExportMetricsServiceRequest messages without length
framing. The protobuf top-level resource_metrics field is repeated, so the
concatenated file can be decoded as one request.
"""

import argparse
import struct
import sys
from typing import Iterator


class MetricsFormatError(ValueError):
    """Raised when metrics.bin is not a valid protobuf stream for this tool."""


def _varint(data: bytes, offset: int) -> tuple[int, int]:
    value = 0
    shift = 0
    while offset < len(data):
        byte = data[offset]
        offset += 1
        value |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return value, offset
        shift += 7
        if shift >= 64:
            raise MetricsFormatError("varint is too long")
    raise MetricsFormatError("truncated varint")


def _fields(data: bytes) -> Iterator[tuple[int, int, int | bytes]]:
    offset = 0
    while offset < len(data):
        tag, offset = _varint(data, offset)
        field_number = tag >> 3
        wire_type = tag & 7
        if field_number == 0:
            raise MetricsFormatError("protobuf field number cannot be zero")

        if wire_type == 0:
            value, offset = _varint(data, offset)
        elif wire_type == 1:
            end = offset + 8
            if end > len(data):
                raise MetricsFormatError("truncated 64-bit field")
            value = data[offset:end]
            offset = end
        elif wire_type == 2:
            length, offset = _varint(data, offset)
            end = offset + length
            if end > len(data):
                raise MetricsFormatError("truncated length-delimited field")
            value = data[offset:end]
            offset = end
        elif wire_type == 5:
            end = offset + 4
            if end > len(data):
                raise MetricsFormatError("truncated 32-bit field")
            value = data[offset:end]
            offset = end
        else:
            raise MetricsFormatError(f"unsupported protobuf wire type {wire_type}")

        yield field_number, wire_type, value


def _messages(data: bytes, field_number: int) -> Iterator[bytes]:
    for number, wire_type, value in _fields(data):
        if number == field_number:
            if wire_type != 2:
                raise MetricsFormatError(f"field {field_number} is not a message")
            yield value  # type: ignore[misc]


def _metric_name(metric: bytes) -> str | None:
    for number, wire_type, value in _fields(metric):
        if number == 1:
            if wire_type != 2:
                raise MetricsFormatError("metric name is not a string")
            return value.decode("utf-8")  # type: ignore[union-attr]
    return None


def _number_datapoint_values(datapoint: bytes) -> Iterator[int | float]:
    for number, wire_type, value in _fields(datapoint):
        if number == 4:  # double_value
            if wire_type != 1:
                raise MetricsFormatError("double value has the wrong wire type")
            yield struct.unpack("<d", value)[0]  # type: ignore[arg-type]
        elif number == 6:  # int_value
            # OTel declares as_int as sfixed64, not int64.
            if wire_type != 1:
                raise MetricsFormatError("int value has the wrong wire type")
            yield struct.unpack("<q", value)[0]  # type: ignore[arg-type]


def memory_values(data: bytes, metric_name: str = "sandbox.memory.usage") -> list[int | float]:
    """Return all datapoint values for *metric_name* in a metrics.bin stream."""
    values: list[int | float] = []
    for resource_metrics in _messages(data, 1):
        for scope_metrics in _messages(resource_metrics, 2):
            for metric in _messages(scope_metrics, 2):
                if _metric_name(metric) != metric_name:
                    continue
                for number, wire_type, series in _fields(metric):
                    if number not in (5, 7):  # gauge or sum
                        continue
                    if wire_type != 2:
                        raise MetricsFormatError("metric data is not a message")
                    for datapoint in _messages(series, 1):
                        values.extend(_number_datapoint_values(datapoint))
    return values


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("metrics_bin", help="path to serialized metrics.bin")
    args = parser.parse_args(argv)

    try:
        with open(args.metrics_bin, "rb") as metrics_file:
            values = memory_values(metrics_file.read())
    except (OSError, MetricsFormatError, UnicodeDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    if not values:
        print("error: no sandbox.memory.usage datapoints found", file=sys.stderr)
        return 1

    average = sum(values) / len(values)
    print(f"samples: {len(values)}")
    print(f"average_bytes: {average:.2f}")
    print(f"average_mib: {average / (1024 * 1024):.6f}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
