import re
import struct
import sys
from pathlib import Path

VERSION = "16.0.0"
MAGIC = b"EGGUCA16"
ELEMENT = re.compile(r"\[([.*])([0-9A-F]{4})\.([0-9A-F]{4})\.([0-9A-F]{4})\]")


def packed(variable, primary, secondary, tertiary):
    if secondary > 0x1FF or tertiary > 0x1F or primary > 0xFFFF:
        raise ValueError(f"weights out of range: {primary:04X}.{secondary:04X}.{tertiary:04X}")
    return (int(variable) << 31) | (primary << 15) | (secondary << 5) | tertiary


def read_allkeys(path):
    singles = {}
    contractions = {}
    implicit = []
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        text = line.split("#", 1)[0].strip()
        if not text:
            continue
        if text.startswith("@implicitweights"):
            ranges, base = text[len("@implicitweights"):].split(";")
            low, high = ranges.strip().split("..")
            implicit.append((int(low, 16), int(high, 16), int(base.strip(), 16)))
            continue
        if text.startswith("@"):
            continue
        codes, weights = text.split(";", 1)
        sequence = tuple(int(code, 16) for code in codes.split())
        elements = [
            packed(mark == "*", int(primary, 16), int(secondary, 16), int(tertiary, 16))
            for mark, primary, secondary, tertiary in ELEMENT.findall(weights)
        ]
        if len(sequence) == 1:
            singles[sequence[0]] = elements
        else:
            contractions[sequence] = elements
    return singles, contractions, implicit


def main(allkeys, output):
    singles, contractions, implicit = read_allkeys(allkeys)
    elements = []
    single_rows = []
    for code in sorted(singles):
        start = len(elements)
        elements.extend(singles[code])
        single_rows.append((code, (start << 5) | len(singles[code])))
    sequence_codes = []
    contraction_rows = []
    for sequence in sorted(contractions):
        start = len(elements)
        elements.extend(contractions[sequence])
        contraction_rows.append((len(sequence_codes), len(sequence), (start << 5) | len(contractions[sequence])))
        sequence_codes.extend(sequence)
    out = bytearray(MAGIC)
    out += struct.pack("<5I", len(single_rows), len(contraction_rows), len(sequence_codes), len(elements), len(implicit))
    for code, entry in single_rows:
        out += struct.pack("<2I", code, entry)
    for start, length, entry in contraction_rows:
        out += struct.pack("<3I", start, length, entry)
    for code in sequence_codes:
        out += struct.pack("<I", code)
    for element in elements:
        out += struct.pack("<I", element)
    for low, high, base in implicit:
        out += struct.pack("<3I", low, high, base)
    Path(output).write_bytes(bytes(out))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
