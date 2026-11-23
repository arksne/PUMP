"""Минимальный WASM-провайдер для сквозной проверки ABI, плеера и HTTP Range.

Модуль возвращает заданные JSON-ответы. Он намеренно не является источником музыки.
"""
import json


def leb(value, signed=False):
    result = bytearray()
    while True:
        byte = value & 127
        value >>= 7
        done = (value == 0 and (not signed or not byte & 64)) or (signed and value == -1 and byte & 64)
        result.append(byte if done else byte | 128)
        if done:
            return bytes(result)


def vector(items):
    return leb(len(items)) + b"".join(items)


def name(value):
    value = value.encode()
    return leb(len(value)) + value


def module(search, resolve):
    search = json.dumps(search, separators=(",", ":")).encode()
    resolve = json.dumps(resolve, separators=(",", ":")).encode()
    assert len(search) < 4096 and len(resolve) < 4096
    sections = []

    def section(kind, body):
        sections.append(bytes([kind]) + leb(len(body)) + body)

    section(1, vector([b"\x60\x00\x01\x7f", b"\x60\x01\x7f\x01\x7f", b"\x60\x02\x7f\x7f\x01\x7e"]))
    section(3, vector([leb(index) for index in [0, 1, 2, 2]]))
    section(5, b"\x01\x00\x01")
    section(7, vector([name("memory") + b"\x02\x00"] + [name(key) + b"\x00" + leb(index) for index, key in enumerate(["plugin_abi_version", "alloc", "search", "resolve"])]))
    bodies = []
    for opcode, value in [(0x41, 1), (0x41, 8192), (0x42, len(search)), (0x42, (4096 << 32) | len(resolve))]:
        body = b"\x00" + bytes([opcode]) + leb(value, signed=True) + b"\x0b"
        bodies.append(leb(len(body)) + body)
    section(10, vector(bodies))
    section(11, vector([b"\x00\x41" + leb(offset, signed=True) + b"\x0b" + leb(len(data)) + data for offset, data in [(0, search), (4096, resolve)]]))
    return b"\x00asm\x01\x00\x00\x00" + b"".join(sections)
