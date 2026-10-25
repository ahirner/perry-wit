"""Compare compiled guest property layouts without changing the production runtime."""

import argparse
import hashlib
import importlib.util
import json
import platform
from pathlib import Path
import shutil
import statistics
import subprocess
import sys

sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location("bench_perry", ROOT / "scripts/bench-perry.py")
BENCH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BENCH)
STORES = ("Dictionary", "SeparateKeys", "CompactKeys", "InternedKeys", "SharedShapes")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--compiler", type=Path, default=ROOT / "target/debug/perry-wit")
    parser.add_argument("--output", type=Path, default=ROOT / "artifacts/property-layouts")
    parser.add_argument("--runs", type=int, default=7)
    parser.add_argument("--objects", type=int, default=10000)
    parser.add_argument("--fields", type=int, nargs="+", default=[8, 20, 64])
    args = parser.parse_args()
    if args.runs < 1 or args.objects < 1 or any(n < 1 for n in args.fields):
        parser.error("runs, objects and fields must be positive")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    compiler = output / "compiler"
    shutil.copy2(args.compiler, compiler)
    template = Path(__file__).with_name("layouts.ts").read_text()
    wit = output / "wit"
    wit.mkdir(exist_ok=True)
    (wit / "world.wit").write_text("package perry:layout-probe; world benchmark { include wasi:cli/command@0.3.0; }\n")
    report = {
        "compiler_sha256": hashlib.sha256(compiler.read_bytes()).hexdigest(),
        "template_sha256": hashlib.sha256(template.encode()).hexdigest(),
        "revision": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "platform": platform.platform(),
        "working_tree_dirty": bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True).strip()),
        "node": subprocess.check_output(["node", "--version"], text=True).strip(),
        "wasmtime": subprocess.check_output(["wasmtime", "--version"], text=True).strip(),
        "runs": args.runs,
        "objects": args.objects,
        "results": [],
    }
    for fields in args.fields:
        for reverse in (False, True):
            for store in STORES:
                directory = output / f"{store}-{fields}-{'alternating' if reverse else 'ordered'}"
                directory.mkdir(exist_ok=True)
                source = template.replace("__STORE__", store).replace("__FIELDS__", str(fields)).replace("__OBJECTS__", str(args.objects)).replace("__REVERSE__", str(reverse).lower())
                entry = directory / "probe.ts"
                entry.write_text(source)
                BENCH.execute([str(compiler), str(entry), "--wit", str(wit), "--world", "benchmark", "-o", "probe.wasm"], directory, directory / "compile")
                row = {"store": store, "fields": fields, "order": "alternating" if reverse else "ordered", "bytes": (directory / "probe.wasm").stat().st_size, "samples": {"node": [], "wasmtime": []}}
                expected = fields * args.objects * args.objects - args.objects
                for run in range(args.runs):
                    for engine in (("node", "wasmtime") if run % 2 else ("wasmtime", "node")):
                        command = ["node", str(entry)] if engine == "node" else ["wasmtime", "run", "-C", "cache=n", "-S", "p3=y", "-W", "component-model-async=y", "probe.wasm"]
                        stdout, memory = BENCH.execute(command, directory, directory / f"{engine}-{run}")
                        values = dict(line.split(":", 1) for line in stdout.splitlines())
                        if int(values["checksum"]) != expected:
                            raise RuntimeError(f"{directory.name}: {engine} checksum mismatch: {stdout}")
                        row["samples"][engine].append({"ms": int(values["elapsed"]), "peak_rss_bytes": memory})
                row["median_ms"] = {engine: statistics.median(sample["ms"] for sample in samples) for engine, samples in row["samples"].items()}
                report["results"].append(row)
                (output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
                print(directory.name, row["median_ms"], flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
