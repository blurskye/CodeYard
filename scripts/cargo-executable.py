#!/usr/bin/env python3
"""Print the unique executable Cargo reported for a named binary target."""
import json
import sys

paths = {
    message["executable"]
    for line in sys.stdin
    if (message := json.loads(line)).get("reason") == "compiler-artifact"
    and message["target"]["name"] == sys.argv[1]
    and "bin" in message["target"]["kind"]
    and message.get("executable")
}
if len(paths) != 1:
    sys.exit(f"Expected one {sys.argv[1]} executable from Cargo")
print(paths.pop())
