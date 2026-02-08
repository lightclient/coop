#!/usr/bin/env bash
# Find .rs files on disk that aren't declared as modules.
# These are invisible to the compiler (no dead_code warning, no compilation).
set -euo pipefail

found=0
for crate_dir in crates/*/; do
    src_dir="$crate_dir/src"
    [ -d "$src_dir" ] || continue

    find "$src_dir" -name '*.rs' | while read -r file; do
        rel=$(realpath --relative-to="$src_dir" "$file")
        base=$(basename "$rel")

        # lib.rs, main.rs, mod.rs are always roots
        [ "$base" = "lib.rs" ] || [ "$base" = "main.rs" ] || [ "$base" = "mod.rs" ] && continue

        dir=$(dirname "$rel")
        stem=$(basename "$rel" .rs)

        if [ "$dir" = "." ]; then
            parent="$src_dir/lib.rs"
            [ -f "$parent" ] || parent="$src_dir/main.rs"
        else
            parent="$src_dir/$dir/mod.rs"
            [ -f "$parent" ] || parent="$src_dir/$dir.rs"
        fi

        if [ -f "$parent" ]; then
            if ! grep -qE "^\s*#?\s*(pub(\(.*\))?\s+)?mod\s+$stem\b" "$parent"; then
                echo "ORPHAN: $file (not declared in $parent)"
                found=1
            fi
        else
            echo "ORPHAN: $file (no parent module found)"
            found=1
        fi
    done
done

if [ "$found" -eq 0 ]; then
    echo "✅ no orphan .rs files"
fi
