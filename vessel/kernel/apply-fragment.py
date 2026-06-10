#!/usr/bin/env python3
"""Apply a kconfig fragment to .config with fragment values winning.

Plain concatenation is unreliable: kconfig keeps the FIRST assignment it
parses for a symbol (observed with CONFIG_BRIDGE on 6.12), so any symbol the
base config already sets would ignore the fragment. Delete base assignments
for fragment symbols, then append the fragment.
"""
import re
import sys

config_path, fragment_path = sys.argv[1], sys.argv[2]

frag_lines = open(fragment_path).read().splitlines()
keys = set()
for line in frag_lines:
    m = re.match(r"^(CONFIG_\w+)=", line) or re.match(r"^# (CONFIG_\w+) is not set", line)
    if m:
        keys.add(m.group(1))

out = []
for line in open(config_path).read().splitlines():
    m = re.match(r"^(CONFIG_\w+)=", line) or re.match(r"^# (CONFIG_\w+) is not set", line)
    if m and m.group(1) in keys:
        continue
    out.append(line)

out.append("")
out.extend(frag_lines)
open(config_path, "w").write("\n".join(out) + "\n")
print(f"applied {len(keys)} fragment symbols over {config_path}")
