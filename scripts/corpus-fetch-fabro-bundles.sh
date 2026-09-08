#!/usr/bin/env bash
# Verify the Fabro black box bundles named in
# crates/fabro/acceptance/bundles.lock.json, digest by digest, and fetch any
# source that is not vendored.
#
#   scripts/corpus-fetch-fabro-bundles.sh            fetch what is not vendored, then verify
#   scripts/corpus-fetch-fabro-bundles.sh --verify   verify only; fetch nothing
#
# A source marked `vendored: true` in the lock has its bundle files tracked in
# this repository under crates/fabro/acceptance/bundles/<id>/<path>
# (PROVENANCE.md there records where they came from). Those files are
# verified in place: every listed file must exist with its mode and SHA-256,
# the bundle hash and the graph-embedded helper hashes must agree, and a
# file under the bundle directory that the lock does not list fails too.
#
# A source without the marker is fetched at its locked revision into
# crates/fabro/acceptance/bundles/.sources/<owner>/<repo> (depth 1) and its
# bundle files are copied into crates/fabro/acceptance/bundles/<id>/ with
# their modes before the same verification. FABRO_BUNDLE_SOURCE_<OWNER>_<REPO>
# (for example FABRO_BUNDLE_SOURCE_LITHOSCOMPUTER_CODE_REVIEW) overrides one
# source URL with a local path. Today every source is vendored, so nothing
# is fetched.
#
# Any missing file, digest drift, extra file, inconsistent hash, or
# unreachable source fails the script. No bundle is skipped silently.
set -euo pipefail
cd "$(dirname "$0")/.."

LOCK=crates/fabro/acceptance/bundles.lock.json
ROOT=crates/fabro/acceptance/bundles
SOURCES="$ROOT/.sources"
VERIFY_ONLY=0
for arg in "$@"; do
  case "$arg" in
    --verify) VERIFY_ONLY=1 ;;
    *) echo "usage: $0 [--verify]" >&2; exit 2 ;;
  esac
done

command -v python3 >/dev/null || { echo "error: python3 is required" >&2; exit 1; }
[ -f "$LOCK" ] || { echo "error: $LOCK is missing" >&2; exit 1; }

# Fetch every source that is not vendored, at its revision.
if [ "$VERIFY_ONLY" -eq 0 ]; then
  while IFS=$'\t' read -r repository url revision visibility; do
    dir="$SOURCES/$repository"
    env_name="FABRO_BUNDLE_SOURCE_$(echo "$repository" | tr '[:lower:]/-' '[:upper:]__')"
    source="${!env_name:-$url}"
    if [ "$(git -C "$dir" rev-parse HEAD 2>/dev/null)" = "$revision" ]; then
      echo "$repository already at $revision"
      continue
    fi
    mkdir -p "$dir"
    git -C "$dir" init -q 2>/dev/null || true
    if ! git -C "$dir" fetch -q --depth 1 "$source" "$revision"; then
      {
        echo "error: could not fetch $repository at $revision from $source"
        echo "  visibility: $visibility"
        if [ "$visibility" = private ]; then
          echo "  Use an SSH agent with read access, or set $env_name to a local checkout."
        fi
        echo "  The bundle set is required: no bundle is skipped."
      } >&2
      exit 1
    fi
    git -C "$dir" checkout -qf FETCH_HEAD
    [ "$(git -C "$dir" rev-parse HEAD)" = "$revision" ] || {
      echo "error: $repository checkout is not at $revision" >&2
      exit 1
    }
    echo "$repository fetched at $revision"
  done < <(python3 -c '
import json, sys
lock = json.load(open(sys.argv[1]))
for key, source in lock["sources"].items():
    if source.get("vendored"):
        continue
    print(source["repository"], source["url"], source["revision"],
          source.get("visibility", "unknown"), sep="\t")
' "$LOCK")
fi

# Copy what was fetched, then verify every bundle in place.
python3 - "$LOCK" "$ROOT" "$SOURCES" <<'EOF2'
import hashlib
import json
import os
import shutil
import stat
import sys

lock_path, root, sources = sys.argv[1:4]
lock = json.load(open(lock_path))
failures = []


def sha256(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def bundle_hash(files):
    lines = []
    for f in files:
        if f["kind"] == "symlink":
            lines.append(f"symlink {f['target']} {f['path']}")
        else:
            lines.append(f"{f['mode']} {f['sha256']} {f['path']}")
    return hashlib.sha256(("\n".join(lines) + "\n").encode()).hexdigest()


def mode_of(path):
    return "0755" if os.stat(path).st_mode & stat.S_IXUSR else "0644"


def copy_from_source(bundle, src_root, dest_root, problems):
    """Materialize a fetched (not vendored) bundle from its source checkout."""
    if os.path.isdir(dest_root):
        shutil.rmtree(dest_root)
    for entry in bundle["files"]:
        src = os.path.join(src_root, entry["path"])
        dest = os.path.join(dest_root, entry["path"])
        os.makedirs(os.path.dirname(dest), exist_ok=True)
        if entry["kind"] == "symlink":
            if not os.path.islink(src) or os.readlink(src) != entry["target"]:
                problems.append(f"symlink {entry['path']} missing or points elsewhere in the source")
                continue
            os.symlink(entry["target"], dest)
            continue
        if not os.path.isfile(src):
            problems.append(f"missing in the source: {entry['path']}")
            continue
        shutil.copyfile(src, dest)
        os.chmod(dest, 0o755 if entry["mode"] == "0755" else 0o644)


def verify_in_place(bundle, dest_root, problems):
    """Check every listed file under the bundle directory, and that nothing else is there."""
    listed = set()
    for entry in bundle["files"]:
        path = os.path.join(dest_root, entry["path"])
        listed.add(os.path.normpath(entry["path"]))
        if entry["kind"] == "symlink":
            if not os.path.islink(path):
                problems.append(f"missing symlink {entry['path']}")
            elif os.readlink(path) != entry["target"]:
                problems.append(f"symlink {entry['path']} points to {os.readlink(path)}, not {entry['target']}")
            continue
        if os.path.islink(path) or not os.path.isfile(path):
            problems.append(f"missing {entry['path']}")
            continue
        digest = sha256(path)
        if digest != entry["sha256"]:
            problems.append(f"digest drift {entry['path']}: {digest} != {entry['sha256']}")
        if mode_of(path) != entry["mode"]:
            problems.append(f"mode {entry['path']}: {mode_of(path)} != {entry['mode']}")
    for dirpath, dirnames, filenames in os.walk(dest_root):
        dirnames[:] = [d for d in dirnames if d != "__pycache__"]
        for name in filenames:
            rel = os.path.normpath(os.path.relpath(os.path.join(dirpath, name), dest_root))
            if rel not in listed and not name.endswith(".pyc"):
                problems.append(f"extra file not in the lock: {rel}")


for bundle in lock["bundles"]:
    source = lock["sources"][bundle["source"]]
    dest_root = os.path.join(root, bundle["id"])
    problems = []
    vendored = bool(source.get("vendored"))
    if not vendored:
        copy_from_source(bundle, os.path.join(sources, source["repository"]), dest_root, problems)
    if not os.path.isdir(dest_root):
        problems.append(f"bundle directory {dest_root} is missing"
                        + (" (the vendored tree is incomplete)" if vendored else ""))
    else:
        verify_in_place(bundle, dest_root, problems)
    computed = bundle_hash(bundle["files"])
    if computed != bundle["bundle_hash"]:
        problems.append(f"bundle hash {computed} != {bundle['bundle_hash']} (lock is inconsistent)")
    for name, value in bundle["dependencies"]["helper_hashes_embedded_in_graph"].items():
        entry = next((f for f in bundle["files"] if f["path"] == name), None)
        if entry is None or entry.get("sha256") != value:
            problems.append(f"graph-embedded hash for {name} does not match the bundle file")
    if bundle["dependencies"]["missing_from_bundle"]:
        problems.append(f"unresolved dependencies: {bundle['dependencies']['missing_from_bundle']}")
    origin = "vendored" if vendored else "fetched"
    if problems:
        failures.append(bundle["id"])
        print(f"FAIL {bundle['id']} ({origin})")
        for p in problems:
            print(f"     {p}")
    else:
        print(f"ok   {bundle['id']} ({len(bundle['files'])} files, {bundle['status']}, {origin})")

if failures:
    print(f"{len(failures)} bundle(s) failed verification: {', '.join(failures)}", file=sys.stderr)
    sys.exit(1)
print(f"{len(lock['bundles'])} bundles verified under {root}; fabro {lock['fabro_reference']['commit']}")
EOF2
