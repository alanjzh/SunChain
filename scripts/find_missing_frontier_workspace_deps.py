#!/usr/bin/env python3
"""
查找 crate/frontier 中依赖于 workspace.dependencies 的 crate，
并列出在项目根 Cargo.toml 的 [workspace.dependencies] 中缺失的项。
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
# 1) frontier workspace.dependencies 对比根 workspace.dependencies
for k, v in frontier_ws_deps.items():
    if k not in root_ws_deps:
        missing[k] = v

# 2) 查找 crate/frontier 下显式使用 `workspace = true` 的依赖
uses_workspace = {}
for t in FRONTIER.rglob("Cargo.toml"):
    c = toml.load(t)
    for sec in ("dependencies", "dev-dependencies", "build-dependencies"):
        deps = c.get(sec, {}) or {}
        for name, val in deps.items():
            if isinstance(val, dict) and val.get("workspace") is True:
                uses_workspace.setdefault(name, set()).add(str(t))

# 把 uses_workspace 中但不在根 workspace.dependencies 的列出来
for name, locations in uses_workspace.items():
    if name not in root_ws_deps and name not in missing:
        # try to get version from frontier workspace deps if present
        info = frontier_ws_deps.get(name, "<no entry in frontier workspace.dependencies>")
        missing[name] = info

if not missing:
    print("No missing workspace.dependencies detected (root already contains frontier's workspace deps).")
    sys.exit(0)

print("Missing workspace.dependencies entries to add to root Cargo.toml:\n")
for k, v in missing.items():
    print(f"# {k}")
    if isinstance(v, dict):
        # print as toml-ish
        inner = ", ".join(f'{kk} = "{vv}"' for kk,vv in v.items())
        print(f'{k} = {{ {inner} }}\n')
    else:
        print(f'{k} = "{v}"\n')

print("Also note these crates declare `workspace = true` (locations):")
for name, locs in uses_workspace.items():
    if name in missing:
        print(f"- {name}:")
        for l in sorted(locs):
            print(f"    {l}")