from pathlib import Path

OLD_REVISION = "9483b92c1fb259f598858fdd2bef66417d87fb2c"
NEW_REVISION = "2f62e40932a0fcb8b9bf1b4c84473e34fa3c51c7"

for path, expected_occurrences in [
    (Path("Cargo.toml"), 1),
    (Path("Cargo.lock"), 2),
]:
    text = path.read_text(encoding="utf-8")
    actual = text.count(OLD_REVISION)
    if actual != expected_occurrences:
        raise RuntimeError(
            f"{path}: expected {expected_occurrences} old flags2env revision "
            f"occurrence(s), found {actual}"
        )
    path.write_text(text.replace(OLD_REVISION, NEW_REVISION), encoding="utf-8")
