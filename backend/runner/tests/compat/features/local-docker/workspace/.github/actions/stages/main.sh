#!/bin/sh
set -eu
echo "stage=main-$1" >> "$GITHUB_STATE"
echo "compat:main-$1-$2-$INPUT_ARGS"
