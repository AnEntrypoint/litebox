#!/usr/bin/env python3
"""Fetch any Docker/OCI image by name and produce a single, litebox-ready,
syscall-rewritten layer tar -- the one-command replacement for manually
chaining pull_oci_image.py then batch_rewrite_layer.py.

Pipeline (both stages are the same tested logic those two scripts use,
imported directly, not reimplemented):
  1. Pull + merge all layers of <image>:<tag> via the raw Registry V2 HTTP
     API (no docker CLI needed), honoring OCI whiteout-file semantics
     (.wh.<name> deletes <name>; .wh..wh..opq marks a directory opaque).
     Never extracts to the host filesystem -- merges purely at the
     tar-stream level, since this project's Windows hosts cannot create
     symlinks without elevation and would silently flatten every symlink
     entry on a real extract+re-tar round trip (see AGENTS.md).
  2. Batch-rewrite every real (non-symlink) ELF in the merged tar with
     litebox_syscall_rewriter.exe, so every package-manager-installed binary
     actually has its syscall sites hooked before litebox ever tries to run
     it (an unrewritten binary #UD-crashes on its first syscall).

Usage:
  python fetch_container.py <repo>:<tag> [-o OUTPUT.tar] [--rewriter PATH]
  e.g. python fetch_container.py linuxserver/webtop:alpine-mate
       python fetch_container.py alpine:latest -o C:/dev/litebox-alpine/alpine.tar

Defaults the output path to <repo-basename>_<tag>.tar under
C:/dev/litebox-images/ (a durable, non-scratch sibling directory -- mirrors
this project's own C:/dev/litebox-webtop/ convention for the canonical
webtop pull, so a fetched image survives a Temp-directory cleanup) unless
-o/--output is given explicitly.

Docker Hub's OFFICIAL images (alpine, ubuntu, busybox, ...) live under the
"library/" namespace on the real registry API even though `docker pull`
hides this -- use library/alpine:latest here, not alpine:latest, or the
registry returns 401 Unauthorized (confirmed live: bare "alpine" fails,
"library/alpine" succeeds). Non-official images use their real path as-is,
e.g. linuxserver/webtop:alpine-mate needs no prefix.
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import pull_oci_image
import batch_rewrite_layer


DEFAULT_IMAGE_DIR = os.path.join("C:\\", "dev", "litebox-images")


def default_output_path(repo, tag):
    repo_basename = repo.rsplit("/", 1)[-1]
    safe_tag = tag.replace(":", "_").replace("/", "_")
    fname = f"{repo_basename}_{safe_tag}.tar"
    return os.path.join(DEFAULT_IMAGE_DIR, fname)


def default_rewriter_path():
    return (
        r"target\release\litebox_syscall_rewriter.exe"
        if os.name == "nt"
        else "target/release/litebox_syscall_rewriter.exe"
    )


def parse_image_ref(ref):
    """Split 'repo:tag' into (repo, tag), defaulting tag to 'latest' if
    omitted. A bare repo may itself contain no colon (registry namespaces
    use '/', not ':', so the only ':' in a normal ref is the tag separator)."""
    if ":" in ref:
        repo, tag = ref.rsplit(":", 1)
    else:
        repo, tag = ref, "latest"
    return repo, tag


def main():
    parser = argparse.ArgumentParser(
        description=(
            "Fetch a Docker/OCI image and produce a litebox-ready, "
            "syscall-rewritten layer tar in one step."
        ),
        epilog=(
            "Examples:\n"
            "  fetch_container.py linuxserver/webtop:alpine-mate\n"
            "  fetch_container.py alpine:latest -o C:/dev/litebox-alpine/alpine.tar\n"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "image",
        help="Image reference as repo:tag, e.g. linuxserver/webtop:alpine-mate "
             "(tag defaults to 'latest' if omitted)",
    )
    parser.add_argument(
        "-o", "--output",
        help="Output tar path. Defaults to "
             f"{DEFAULT_IMAGE_DIR}\\<repo-basename>_<tag>.tar",
    )
    parser.add_argument(
        "--rewriter",
        help="Path to litebox_syscall_rewriter.exe "
             f"(default: {default_rewriter_path()})",
    )
    parser.add_argument(
        "--keep-merged",
        action="store_true",
        help="Keep the intermediate pulled-and-merged (pre-rewrite) tar "
             "alongside the final output, named <output>.merged.tar",
    )
    args = parser.parse_args()

    repo, tag = parse_image_ref(args.image)
    output_path = args.output or default_output_path(repo, tag)
    rewriter_path = args.rewriter or default_rewriter_path()

    if not os.path.isfile(rewriter_path):
        print(f"error: rewriter not found: {rewriter_path}", file=sys.stderr)
        print(
            "Build it first: cargo build --release -p litebox_syscall_rewriter",
            file=sys.stderr,
        )
        sys.exit(1)

    out_dir = os.path.dirname(os.path.abspath(output_path))
    os.makedirs(out_dir, exist_ok=True)

    merged_path = (
        f"{output_path}.merged.tar" if args.keep_merged
        else output_path + ".merged.tmp"
    )

    print(f"=== stage 1/2: pulling and merging {repo}:{tag} ===")
    try:
        pull_oci_image.pull_and_merge(repo, tag, merged_path)
    except Exception as e:
        print(f"error: pull/merge failed: {e}", file=sys.stderr)
        sys.exit(1)

    print(f"=== stage 2/2: rewriting ELF syscall sites ===")
    try:
        batch_rewrite_layer.rewrite_layer(merged_path, output_path, rewriter_path)
    except Exception as e:
        print(f"error: rewrite failed: {e}", file=sys.stderr)
        sys.exit(1)
    finally:
        if not args.keep_merged and os.path.exists(merged_path):
            os.remove(merged_path)

    print(f"=== done: {output_path} ===")
    print(
        f"Boot it with: litebox_runner_linux_on_windows_userland.exe "
        f"--initial-files {output_path} -- <program> [args...]"
    )


if __name__ == "__main__":
    main()
