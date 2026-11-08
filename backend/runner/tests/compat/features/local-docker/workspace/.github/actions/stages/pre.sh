#!/bin/sh
set -eu
echo "stage=pre-$1" >> "$GITHUB_STATE"
echo "compat:pre-$1"
