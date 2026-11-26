#!/bin/sh
set -eu
if [ "$#" -ne 1 ] || [ -z "$1" ]; then
    echo 'Usage: sh verify.sh /path/to/reconstructed-source' >&2
    exit 1
fi
kit=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
exec python3 "$kit/check_inputs.py" "$1" "$kit/upstream-files.json"
