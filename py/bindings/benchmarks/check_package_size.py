from __future__ import annotations

import argparse
import json
from pathlib import Path
from zipfile import ZipFile


def measure(wheel: Path) -> dict[str, int | str]:
    with ZipFile(wheel) as archive:
        files = [entry for entry in archive.infolist() if not entry.is_dir()]
        native = [
            entry
            for entry in files
            if Path(entry.filename).suffix in {".dylib", ".pyd", ".so"}
        ]
    if len(native) != 1:
        raise ValueError(
            f"{wheel} contains {len(native)} native extensions; expected exactly one"
        )
    return {
        "wheel": str(wheel.resolve()),
        "wheel_bytes": wheel.stat().st_size,
        "unpacked_bytes": sum(entry.file_size for entry in files),
        "native_extension": native[0].filename,
        "native_extension_bytes": native[0].file_size,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("wheels", nargs="+", type=Path)
    args = parser.parse_args()
    thresholds = json.loads(
        Path(__file__).with_name("package_thresholds.json").read_text()
    )
    reports = [measure(wheel) for wheel in args.wheels]
    print(json.dumps(reports, indent=2, sort_keys=True))

    failures = []
    for report in reports:
        for metric, maximum in thresholds.items():
            observed = report[metric]
            if not isinstance(observed, int):
                raise TypeError(f"{metric} did not produce an integer byte count")
            if observed > maximum:
                failures.append(f"{report['wheel']} {metric}: {observed} > {maximum}")
    if failures:
        raise SystemExit("Python package size SLA exceeded:\n" + "\n".join(failures))


if __name__ == "__main__":
    main()
