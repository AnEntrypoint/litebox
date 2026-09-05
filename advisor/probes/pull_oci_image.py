#!/usr/bin/env python3
"""Pull a multi-layer OCI/Docker image via the Registry V2 HTTP API (no docker
CLI needed) and merge its layers into a single, litebox-loadable tar.

Handles OCI whiteout-file semantics (.wh.<name> in a later layer deletes
<name> from the merged result; .wh..wh..opq marks a directory "opaque",
deleting everything under it from earlier layers before this layer's own
entries apply).

Never extracts to the host filesystem with tar -x: this repo's own Windows
host cannot create symlinks without elevation, silently flattening every
symlink entry -- see AGENTS.md. Merges purely at the tar-stream level.

Usage: python pull_oci_image.py <repo> <tag> <output.tar>
  e.g. python pull_oci_image.py linuxserver/webtop alpine-mate out.tar
"""
import sys
import json
import gzip
import io
import tarfile
import urllib.request

REGISTRY = "https://registry-1.docker.io"
AUTH = "https://auth.docker.io/token"


def get_token(repo):
    url = f"{AUTH}?service=registry.docker.io&scope=repository:{repo}:pull"
    with urllib.request.urlopen(url) as r:
        return json.load(r)["token"]


def fetch_json(url, token, accept):
    req = urllib.request.Request(url, headers={
        "Authorization": f"Bearer {token}", "Accept": accept,
    })
    with urllib.request.urlopen(req) as r:
        return json.load(r)


def fetch_bytes(url, token):
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}"})
    with urllib.request.urlopen(req) as r:
        return r.read()


def pull_and_merge(repo, tag, out_path):
    """Pull <repo>:<tag> and write the merged, whiteout-resolved layer tar to
    out_path. Extracted from main() so fetch_container.py can call this
    directly instead of shelling out -- identical logic, just callable."""
    token = get_token(repo)

    index_url = f"{REGISTRY}/v2/{repo}/manifests/{tag}"
    index = fetch_json(index_url, token,
                        "application/vnd.docker.distribution.manifest.list.v2+json,"
                        "application/vnd.oci.image.index.v1+json,"
                        "application/vnd.oci.image.manifest.v1+json,"
                        "application/vnd.docker.distribution.manifest.v2+json")

    if "manifests" in index:
        amd64 = next(m for m in index["manifests"]
                     if m.get("platform", {}).get("architecture") == "amd64"
                     and m.get("platform", {}).get("os") == "linux")
        manifest = fetch_json(
            f"{REGISTRY}/v2/{repo}/manifests/{amd64['digest']}", token,
            "application/vnd.oci.image.manifest.v1+json,"
            "application/vnd.docker.distribution.manifest.v2+json")
    else:
        manifest = index

    layers = manifest["layers"]
    print(f"{len(layers)} layers to merge")

    # path -> tarinfo+data (None data = directory/symlink/etc, handled specially)
    merged = {}  # path -> (tarinfo, data_bytes_or_None)
    order = []   # insertion order, for deterministic output

    for i, layer in enumerate(layers):
        digest = layer["digest"]
        size = layer["size"]
        print(f"[{i+1}/{len(layers)}] fetching {digest} ({size} bytes)...")
        raw = fetch_bytes(f"{REGISTRY}/v2/{repo}/blobs/{digest}", token)
        with tarfile.open(fileobj=io.BytesIO(gzip.decompress(raw)), mode="r") as lt:
            for member in lt:
                name = member.name.lstrip("./")
                base = name.rsplit("/", 1)
                dirpart = base[0] if len(base) == 2 else ""
                fname = base[-1]

                if fname == ".wh..wh..opq":
                    # opaque dir marker: drop every existing entry under dirpart
                    prefix = dirpart + "/" if dirpart else ""
                    to_remove = [p for p in merged if p.startswith(prefix) and p != dirpart]
                    for p in to_remove:
                        del merged[p]
                        if p in order:
                            order.remove(p)
                    continue
                if fname.startswith(".wh."):
                    # whiteout: delete the named entry from earlier layers
                    target = fname[len(".wh."):]
                    target_path = f"{dirpart}/{target}" if dirpart else target
                    merged.pop(target_path, None)
                    if target_path in order:
                        order.remove(target_path)
                    continue

                data = None
                if member.isfile():
                    f = lt.extractfile(member)
                    data = f.read() if f else b""
                if name not in merged:
                    order.append(name)
                merged[name] = (member, data)

    print(f"merged: {len(order)} entries, writing {out_path}...")
    with tarfile.open(out_path, "w") as out:
        for name in order:
            member, data = merged[name]
            member.name = name
            if data is not None:
                out.addfile(member, io.BytesIO(data))
            else:
                out.addfile(member)
    print("done")


def main():
    if len(sys.argv) < 4:
        print(__doc__)
        sys.exit(1)
    pull_and_merge(sys.argv[1], sys.argv[2], sys.argv[3])


if __name__ == "__main__":
    main()
