#!/bin/bash
# Run a scenario on River and on the standalone compositor and put the
# screenshots side by side: tools/e2e/compare.sh <scenario> [out-root]
HERE=$(cd "$(dirname "$0")" && pwd)
S=$1; NAME=$(basename "${S%.*}"); OUT=${2:-/tmp/cne2e-out}/$NAME
"$HERE/run.sh" river "$S" "$OUT/river"
"$HERE/run.sh" smithay "$S" "$OUT/smithay"
for f in "$OUT"/river/*.png; do
    b=$(basename "$f")
    [ -f "$OUT/smithay/$b" ] || continue
    convert "$f" "$OUT/smithay/$b" -resize 50% +append -bordercolor red -border 2 "$OUT/cmp-$b"
    diff=$(compare -metric AE -fuzz 2% "$f" "$OUT/smithay/$b" null: 2>&1)
    echo "$NAME/$b: differing pixels: $diff"
done
