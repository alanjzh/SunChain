#!/usr/bin/env python3
"""
把 workspace.dependencies 重新排序并输出到屏幕：
 - 第一组：所有 path 开头为 "third_party/polkadot-sdk/" 的依赖，按 crate 名字字母序
 - 第二组：所有 path 开头为 "crate/frontier" 的依赖，按 crate 名字字母序
 - 第三组：其余（通常来自 crates.io），按 crate 名字字母序

不修改任何文件，仅在 stdout 打印可直接粘贴的 toml 行（每个依赖一行）。
"""
import toml
from pathlib import Path
import sys

ROOT = Path("/data/jzh/sun_dev/sunchain")
CARGO = ROOT / "Cargo.toml"

if not CARGO.exists():
    print("Cargo.toml not found:", CARGO, file=sys.stderr)
    sys.exit(1)

doc = toml.load(CARGO)
ws = doc.get("workspace", {})
deps = ws.get("dependencies", {})

third = {}
frontier = {}
others = {}

for k, v in deps.items():
    # normalize value printing later
    path = None
    if isinstance(v, dict):
        path = v.get("path", "")
    if isinstance(path, str) and path.startswith("third_party/polkadot-sdk"):
        third[k] = v
    elif isinstance(path, str) and path.startswith("crate/frontier"):
        frontier[k] = v
    else:
        others[k] = v

def fmt_val(v):
    if isinstance(v, dict):
        inner = []
        for kk, vv in v.items():
            if isinstance(vv, bool):
                inner.append(f'{kk} = {str(vv).lower()}')
            elif isinstance(vv, list):
                arr = ", ".join(f'"{x}"' for x in vv)
                inner.append(f'{kk} = [{arr}]')
            else:
                inner.append(f'{kk} = "{vv}"')
        return "{ " + ", ".join(inner) + " }"
    else:
        return f'"{v}"'

def print_group(title, mapping):
    if not mapping:
        return
    # optional header comment
    print(f"# {title}")
    for k in sorted(mapping.keys(), key=lambda s: s.lower()):
        print(f'{k} = {fmt_val(mapping[k])}')
    print()

# 输出顺序： third_party, crate/frontier, crates.io (others)
print("# Reordered workspace.dependencies (grouped)\n")
print_group("path = \"third_party/polkadot-sdk/...\"", third)
print_group("path = \"crate/frontier/...\"", frontier)
print_group("crates.io / other dependencies", others)