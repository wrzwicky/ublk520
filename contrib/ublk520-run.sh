#!/bin/bash
# Find the 520-byte drive's sg node (it can change between boots) and run ublk520 on it.
# Safety: only runs on a device whose logical block length is exactly 520.
# Override with environment variables: UBLK520_MODEL (substring of the lsscsi line), UBLK520_ID.
set -eu
MODEL_MATCH="${UBLK520_MODEL:-MZ-3S910}"
ID="${UBLK520_ID:-0}"

for SG in $(lsscsi -g | awk -v m="$MODEL_MATCH" '$0 ~ m {print $NF}'); do
    if sg_readcap -l "$SG" 2>/dev/null | grep -q 'Logical block length=520'; then
        exec /usr/local/bin/ublk520 --id "$ID" "$SG"
    fi
done
echo "ublk520-run: no 520-byte-sector drive matching '$MODEL_MATCH' found" >&2
exit 1
