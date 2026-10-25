"""Enforce the contract crate's independence from lowering and runtime builds."""
import json
import subprocess

metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--format-version", "1"]))
names = {package["id"]: package["name"] for package in metadata["packages"]}
nodes = {node["id"]: node["dependencies"] for node in metadata["resolve"]["nodes"]}
forbidden = {"perry-wit", "perry-frontend", "perry-hir", "perry-lower", "perry-mir", "perry-codegen", "pliron", "perry-rt-build"}
pending = [identity for identity, name in names.items() if name == "perry-contract"]
visited = set()
while pending:
    identity = pending.pop()
    if identity in visited:
        continue
    visited.add(identity)
    if names[identity] in forbidden:
        raise SystemExit(f"perry-contract depends on forbidden crate {names[identity]}")
    pending.extend(nodes[identity])
print("Contract dependencies exclude lowering and runtime builds.")
