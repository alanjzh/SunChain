#!/usr/bin/env python3
"""
找出 crate/frontier 需要但根 Cargo.toml workspace.dependencies 中缺失的依赖，
并打印出可粘贴的 toml 片段。
"""
import toml, pathlib, sys

ROOT = pathlib.Path.cwd()
FRONTIER = ROOT / "crate" / "frontier"
ROOT_CARGO = ROOT / "Cargo.toml"
FRONTIER_CARGO = FRONTIER / "Cargo.toml"

if not FRONTIER.exists():
    print("ERROR: crate/frontier not found at", FRONTIER); sys.exit(1)
if not FRONTIER_CARGO.exists():
    print("ERROR: frontier Cargo.toml not found at", FRONTIER_CARGO); sys.exit(1)
if not ROOT_CARGO.exists():
    print("ERROR: root Cargo.toml not found at", ROOT_CARGO); sys.exit(1)

root = toml.load(ROOT_CARGO)
frontier = toml.load(FRONTIER_CARGO)

root_ws_deps = root.get("workspace", {}).get("dependencies", {}) or {}
frontier_ws_deps = frontier.get("workspace", {}).get("dependencies", {}) or {}

missing = {}

# 1) frontier workspace.dependencies 中不存在于根 workspace.dependencies 的
for k, v in frontier_ws_deps.items():
    if k not in root_ws_deps:
        missing[k] = v

# 2) 查找 crate/frontier 下声明 `workspace = true` 的依赖
uses_workspace = {}
for t in FRONTIER.rglob("Cargo.toml"):
    c = toml.load(t)
    for sec in ("dependencies", "dev-dependencies", "build-dependencies"):
        deps = c.get(sec, {}) or {}
        for name, val in deps.items():
            if isinstance(val, dict) and val.get("workspace") is True:
                uses_workspace.setdefault(name, []).append(str(t))
                if name not in root_ws_deps and name not in missing:
                    # 如果 frontier root 提供了默认配置，优先使用它
                    info = frontier_ws_deps.get(name, "<no entry in frontier workspace.dependencies>")
                    missing[name] = info

if not missing:
    print("No missing workspace.dependencies detected.")
    sys.exit(0)

print("# Add the following to your root Cargo.toml's [workspace.dependencies]:\n")
for k, v in missing.items():
    print(f"# Source: referenced by frontier (or used with workspace = true)")
    if isinstance(v, dict):
        # Format dict nicely
        inner = ", ".join(f'{kk} = "{vv}"' for kk, vv in v.items())
        print(f'{k} = {{ {inner} }}\n')
    else:
        print(f'{k} = "{v}"\n')

print("Also these crates declare `workspace = true` (locations):")
for name, locs in uses_workspace.items():
    if name in missing:
        print(f"- {name}:")
        for l in locs:
            print(f"    {l}")