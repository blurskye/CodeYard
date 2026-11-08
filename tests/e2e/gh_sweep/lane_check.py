#!/usr/bin/env python3
"""Static done-check for one lane: python3 tests/e2e/gh_sweep/lane_check.py g3
Needs no server and no gh. Exit 0 only when the lane's commands are all classified, each has
a case in cases_<lane>.py, and every flag of each command appears in that command's cases
or in waivers_<lane>.json with a reason."""
import ast
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))


def main(lane):
    from gh_sweep import harness

    commands = json.loads((HERE / "groups.json").read_text())[lane]
    inventory = json.loads((HERE / "inventory.json").read_text())
    path = HERE / f"cases_{lane}.py"
    tree = ast.parse(path.read_text())
    harness.importlib.import_module(f"gh_sweep.cases_{lane}")
    registered = {}
    for case in harness.CASES:
        registered.setdefault(case.command, []).append(case)
    classification = json.loads((HERE / f"classification_{lane}.json").read_text())["commands"]
    waivers = json.loads((HERE / f"waivers_{lane}.json").read_text()).get("flags", {})
    # Flags are matched against the string literals of each case function's source.
    literals = {}
    for node in ast.walk(tree):
        if isinstance(node, ast.FunctionDef):
            literals[node.name] = {n.value for n in ast.walk(node) if isinstance(n, ast.Constant) and isinstance(n.value, str)}
    problems = []
    for command in commands:
        if command not in classification:
            problems.append(f"{command}: not classified")
        cases = registered.get(command, [])
        if not cases:
            problems.append(f"{command}: no case")
        text = " ".join(" ".join(literals.get(c.function.__name__, ())) for c in cases)
        for flag in inventory[command]["flags"]:
            if flag == "--help" or flag in text or flag in waivers.get(command, {}):
                continue
            short = {"-R": "--repo"}.get(flag)
            if short and short in text:
                continue
            problems.append(f"{command}: flag {flag} not exercised")
    outside = sorted(c for c in registered if c not in commands) + sorted(c for c in classification if c not in commands)
    problems += [f"{c}: not in lane {lane}" for c in outside]
    print("\n".join(problems) or f"lane {lane} ok: {len(commands)} commands")
    return int(bool(problems))


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
