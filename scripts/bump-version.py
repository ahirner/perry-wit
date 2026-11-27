"""Update the compiler, WIT and npm package versions in the current checkout."""

import argparse
import json
import re
import subprocess
import tempfile
from pathlib import Path

import tomlkit
from semver import Version


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", type=Version.parse)
    target = parser.parse_args().version
    version = str(target)

    manifest_path = Path("Cargo.toml")
    manifest = tomlkit.loads(manifest_path.read_text())
    package = manifest["package"]
    if target < Version.parse(package["version"]):
        parser.error(f"requested {target} is older than {package['version']}")
    package["version"] = version

    cargo_lock_path = Path("Cargo.lock")
    cargo_lock = tomlkit.loads(cargo_lock_path.read_text())
    compiler, = (
        entry for entry in cargo_lock["package"]
        if entry["name"] == package["name"] and "source" not in entry
    )
    compiler["version"] = version

    sdk = Path("js/packages/perry-wit/package.json")
    npm_lock_path = Path("js/package-lock.json")
    npm_manifest = json.loads(sdk.read_text())
    npm_lock = json.loads(npm_lock_path.read_text())
    npm_manifest["version"] = version
    npm_lock["packages"]["packages/perry-wit"]["version"] = version
    lock_contents = json.dumps(npm_lock, indent=2, ensure_ascii=False) + "\n"

    wit = Path("wit/compiler/world.wit")
    wit_contents, count = re.subn(
        r"(?m)^package perry:compiler@[^;\s]+;$", f"package perry:compiler@{version};", wit.read_text()
    )
    if count != 1:
        parser.error(f"expected one perry:compiler package declaration in {wit}, found {count}")

    flake = Path("flake.nix")
    flake_contents = flake.read_text()
    current_hash = subprocess.check_output(["nix", "eval", "--raw", ".#npm-package.npmDeps.outputHash"], text=True)
    hash_literal = json.dumps(current_hash)
    if flake_contents.count(hash_literal) != 1:
        parser.error(f"expected one literal for the evaluated npm dependency hash {current_hash} in {flake}")

    with tempfile.TemporaryDirectory() as temp:
        lock = Path(temp) / "package-lock.json"
        lock.write_text(lock_contents)
        npm_hash = subprocess.check_output(["prefetch-npm-deps", lock], text=True).strip()

    updates = {
        manifest_path: tomlkit.dumps(manifest),
        cargo_lock_path: tomlkit.dumps(cargo_lock),
        sdk: json.dumps(npm_manifest, indent=2, ensure_ascii=False) + "\n",
        npm_lock_path: lock_contents,
        wit: wit_contents,
        flake: flake_contents.replace(hash_literal, json.dumps(npm_hash)),
    }
    for path, contents in updates.items():
        if path.read_text() != contents:
            path.write_text(contents)


if __name__ == "__main__":
    main()
