import sys
from pathlib import Path

VERSION = "16.0.0"


def read_unicode_data(path):
    classes = {}
    decompositions = {}
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        fields = line.split(";")
        if len(fields) < 6:
            continue
        code = int(fields[0], 16)
        if int(fields[3]) != 0:
            classes[code] = int(fields[3])
        mapping = fields[5].strip()
        if mapping:
            compat = mapping.startswith("<")
            parts = mapping.split(">", 1)[1].split() if compat else mapping.split()
            decompositions[code] = (compat, [int(part, 16) for part in parts])
    return classes, decompositions


def read_exclusions(path):
    excluded = set()
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        text = line.split("#", 1)[0].strip()
        if text:
            excluded.add(int(text, 16))
    return excluded


def read_folding(path):
    folding = {}
    for line in Path(path).read_text(encoding="utf-8").splitlines():
        text = line.split("#", 1)[0].strip()
        if not text:
            continue
        code, status, mapping = [field.strip() for field in text.split(";")[:3]]
        if status in ("C", "F"):
            folding[int(code, 16)] = [int(part, 16) for part in mapping.split()]
    return folding


def main(data_directory, output):
    directory = Path(data_directory)
    classes, decompositions = read_unicode_data(directory / "UnicodeData.txt")
    excluded = read_exclusions(directory / "CompositionExclusions.txt")
    folding = read_folding(directory / "CaseFolding.txt")
    data = []
    entries = []
    for code in sorted(decompositions):
        compat, parts = decompositions[code]
        entries.append((code, len(data), len(parts), compat))
        data.extend(parts)
    compositions = []
    for code in sorted(decompositions):
        compat, parts = decompositions[code]
        if compat or len(parts) != 2 or code in excluded:
            continue
        if classes.get(code, 0) != 0 or classes.get(parts[0], 0) != 0:
            continue
        compositions.append((parts[0], parts[1], code))
    compositions.sort()
    folding_data = []
    folding_entries = []
    for code in sorted(folding):
        folding_entries.append((code, len(folding_data), len(folding[code])))
        folding_data.extend(folding[code])
    lines = [f'pub(crate) const UNICODE_VERSION: &str = "{VERSION}";', ""]

    def table(name, row_type, rows):
        lines.append(f"pub(crate) static {name}: [{row_type}; {len(rows)}] = [")
        for row in rows:
            lines.append(f"    {row},")
        lines.append("];")
        lines.append("")

    table("COMBINING_CLASSES", "(u32, u8)", [f"(0x{code:X}, {value})" for code, value in sorted(classes.items())])
    table("DECOMPOSITIONS", "(u32, u16, u8, bool)", [f"(0x{code:X}, {offset}, {length}, {str(compat).lower()})" for code, offset, length, compat in entries])
    table("DECOMPOSITION_DATA", "u32", [f"0x{code:X}" for code in data])
    table("COMPOSITIONS", "(u32, u32, u32)", [f"(0x{first:X}, 0x{second:X}, 0x{composite:X})" for first, second, composite in compositions])
    table("CASE_FOLDING", "(u32, u16, u8)", [f"(0x{code:X}, {offset}, {length})" for code, offset, length in folding_entries])
    table("CASE_FOLDING_DATA", "u32", [f"0x{code:X}" for code in folding_data])
    Path(output).write_text("\n".join(lines).rstrip() + "\n", encoding="utf-8")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
