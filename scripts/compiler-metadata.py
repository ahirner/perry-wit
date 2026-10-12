"""Annotate or verify the distribution component; never invoked by the compiler API."""

import argparse
import json
import pathlib
import subprocess
import tomllib


ROOT = pathlib.Path(__file__).resolve().parent.parent


def run(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def source_revision():
    revision = run("git", "rev-parse", "HEAD")
    dirty = run("git", "status", "--porcelain", "--untracked-files=normal")
    return revision + ("-dirty" if dirty else "")


def inspect(path):
    tree = json.loads(run("wasm-tools", "metadata", "show", str(path), "--json"))
    if "component" not in tree:
        raise ValueError("the compiler distribution must be a component")
    return tree["component"]["metadata"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", type=pathlib.Path)
    parser.add_argument("output", type=pathlib.Path, nargs="?")
    parser.add_argument("--revision", help="source revision supplied by a hermetic build")
    args = parser.parse_args()
    package = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]
    fields = {key: package[key] for key in ("name", "description", "version", "homepage")}
    fields.update(source=package["repository"], licenses=package["license"], authors=", ".join(package["authors"]))
    before = inspect(args.input)
    if args.output:
        fields["revision"] = args.revision or source_revision()
        command = ["wasm-tools", "metadata", "add", str(args.input), "-o", str(args.output)]
        for name, value in fields.items():
            command.extend(["--" + name, value])
        command.extend(["--language", "Rust=", "--processed-by", "rustc=" + run("rustc", "--version").removeprefix("rustc "),
                        "--processed-by", "wasm-tools=" + run("wasm-tools", "--version").removeprefix("wasm-tools ")])
        subprocess.run(command, cwd=ROOT, check=True)
    metadata = inspect(args.output or args.input)
    for name, value in fields.items():
        if metadata[name] != value:
            raise ValueError(f"compiler {name}: expected {value!r}, found {metadata[name]!r}")
    if not metadata.get("revision"):
        raise ValueError("compiler source revision is missing")
    producers = dict(metadata["producers"] or [])
    for field, values in before["producers"] or []:
        if not values.items() <= producers.get(field, {}).items():
            raise ValueError(f"existing {field} producers were lost")
    if "Rust" not in producers.get("language", {}) or not all(name in producers.get("processed-by", {}) for name in ("rustc", "wasm-tools")):
        raise ValueError("compiler language/tool provenance is missing")
    print(json.dumps(metadata, indent=2))


if __name__ == "__main__":
    main()
