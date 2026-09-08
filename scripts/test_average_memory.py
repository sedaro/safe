import struct
import unittest

from scripts.average_memory import memory_values
from scripts.replay_metrics import batches, resource_metrics


def varint(value: int) -> bytes:
    result = bytearray()
    while value >= 128:
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def field(number: int, payload: bytes) -> bytes:
    return varint(number << 3 | 2) + varint(len(payload)) + payload


def metric(name: str, value: int) -> bytes:
    datapoint = varint(6 << 3 | 1) + struct.pack("<q", value)
    gauge = field(1, datapoint)
    return field(1, name.encode()) + field(5, gauge)


def request(value: int) -> bytes:
    scope = field(2, metric("sandbox.memory.usage", value))
    resource = field(2, scope)
    return field(1, resource)


class AverageMemoryTest(unittest.TestCase):
    def test_reads_concatenated_requests_and_ignores_other_metrics(self) -> None:
        data = request(100) + request(300)
        self.assertEqual(memory_values(data), [100, 300])

    def test_reads_double_datapoints(self) -> None:
        datapoint = varint(4 << 3 | 1) + struct.pack("<d", 2.5)
        gauge = field(1, datapoint)
        metric_data = field(1, b"sandbox.memory.usage") + field(5, gauge)
        self.assertEqual(memory_values(field(1, field(2, field(2, metric_data)))), [2.5])

    def test_replay_extracts_and_rebatches_resource_metrics(self) -> None:
        data = request(100) + request(300)
        resources = resource_metrics(data)
        replayed = list(batches(resources, 1))
        self.assertEqual(len(replayed), 2)
        self.assertEqual(memory_values(replayed[0]), [100])
        self.assertEqual(memory_values(replayed[1]), [300])


if __name__ == "__main__":
    unittest.main()
