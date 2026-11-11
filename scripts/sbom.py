#!/usr/bin/env python3
"""Write a CycloneDX 1.5 SBOM (JSON, on stdout) for the spared host binary from Cargo.lock.

    python3 -I scripts/sbom.py Cargo.lock 0.1.0 > spared-0.1.0.cdx.json

It lists every package in the lock file with its version, its crates.io package URL and the
SHA-256 that cargo recorded for it. That is the full resolved dependency set for all platforms,
so a given binary links a subset of it. The output has no timestamp and no random serial number (the
serial number is a UUIDv5 of the version, which actions/attest requires): the same lock file
and version always give the same bytes. Used by .github/workflows/release.yml,
which attaches the file to the release and attests it. Needs Python 3.11 (tomllib).
"""
import json
import sys
import tomllib
import uuid

CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"


def component(pkg):
    name, version = pkg["name"], pkg["version"]
    c = {"type": "library", "name": name, "version": version, "bom-ref": f"{name}@{version}"}
    source = pkg.get("source", "")
    if source == CRATES_IO:
        c["purl"] = f"pkg:cargo/{name}@{version}"
    elif source:
        c["properties"] = [{"name": "cargo:source", "value": source}]
    if "checksum" in pkg:
        c["hashes"] = [{"alg": "SHA-256", "content": pkg["checksum"]}]
    return c


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: sbom.py <Cargo.lock> <version>")
    with open(sys.argv[1], "rb") as f:
        packages = tomllib.load(f)["package"]
    root = [p for p in packages if p["name"] == "spared" and "source" not in p]
    if len(root) != 1:
        raise SystemExit("sbom.py: expected exactly one local package named spared in the lock file")
    deps = sorted((p for p in packages if p is not root[0]), key=lambda p: (p["name"], p["version"]))
    bom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "serialNumber": f"urn:uuid:{uuid.uuid5(uuid.NAMESPACE_URL, 'https://github.com/michaelmagan/spared@' + sys.argv[2])}",
        "version": 1,
        "metadata": {
            "component": {"type": "application", "name": "spared", "version": sys.argv[2], "bom-ref": f"spared@{sys.argv[2]}"},
            "tools": [{"name": "scripts/sbom.py"}],
        },
        "components": [component(p) for p in deps],
    }
    json.dump(bom, sys.stdout, indent=1, sort_keys=True)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
