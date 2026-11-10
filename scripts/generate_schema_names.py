"""Regenerate the source-only format-key dictionary without reading any game files."""
from pathlib import Path

KEYS = [
    "ClassName", "LuaParam", "Position", "Type", "XAxis", "YAxis", "ZAxis",
    "Node05", "FenceNode08", "FenceNodeFlag21", "FenceWallFlag12", "Looping",
    "Sound Events", "Game Event Name", "Sound Event Name", "Model", "Texture",
]


def name_hash(name: str) -> int:
    value = 0x811C9DC5
    for byte in name.encode("ascii"):
        value = (((byte | 0x20) ^ value) * 0x01000193) & 0xFFFFFFFF
    return ((value ^ 0x2A) * 0x01000193) & 0xFFFFFFFF


if __name__ == "__main__":
    destination = Path(__file__).resolve().parents[1] / "data" / "hash_names.tsv"
    rows = ["hash\tname\tgrade\tchance\tkinds\tsources"]
    for key in sorted(KEYS, key=name_hash):
        rows.append(f"0x{name_hash(key):08x}\t{key}\tstructural\t-\tformat-key\tsource-schema")
    destination.write_text("\n".join(rows) + "\n", encoding="utf-8")
