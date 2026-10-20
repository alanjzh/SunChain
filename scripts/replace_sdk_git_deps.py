#!/usr/bin/env python3
"""
仅处理 crate/frontier/Cargo.toml 中指向 paritytech/polkadot-sdk 的 git 依赖，
把它们替换为 third_party/polkadot-sdk 下对应的本地 path。

用法：
  # 仅预览（不写入）
  python3 scripts/replace_frontier_git_deps.py

  # 实际写入修改
  python3 scripts/replace_frontier_git_deps.py --apply
"""
import toml
from pathlib import Path
import argparse

REPO_ROOT = Path("/data/jzh/sun_dev/sunchain").resolve()
TARGET = REPO_ROOT / "crate" / "frontier" / "Cargo.toml"
SDK_ROOT = REPO_ROOT / "third_party" / "polkadot-sdk"
GIT_URL = "https://github.com/paritytech/polkadot-sdk"

def find_local_path_for_pkg(pkg_name: str):
    # 在 third_party/polkadot-sdk 下通过 package.name 查找
    for cargo in SDK_ROOT.rglob("Cargo.toml"):
        try:
            data = toml.load(cargo)
        except Exception:
            continue
        if data.get("package", {}).get("name") == pkg_name:
            return str(cargo.parent.relative_to(REPO_ROOT)).replace("\\", "/")
    # 再尝试按目录名匹配
    for d in SDK_ROOT.rglob("*"):
        if d.is_dir() and d.name == pkg_name:
            return str(d.relative_to(REPO_ROOT)).replace("\\", "/")
    return None

def process_dependencies_table(table):
    changed = {}
    for name, val in list(table.items()):
        if isinstance(val, dict):
            if val.get("git") == GIT_URL:
                # try package override first
                pkg_name = val.get("package", name)
                local = find_local_path_for_pkg(pkg_name)
                if local:
                    new = val.copy()
                    # remove git-related keys
                    new.pop("git", None)
                    new.pop("branch", None)
                    new.pop("tag", None)
                    new.pop("rev", None)
                    # set path
                    new["path"] = local
                    table[name] = new
                    changed[name] = local
                else:
                    # not found -> leave as-is but report
                    changed[name] = None
    return changed

def main(dry_run=True):
    if not TARGET.exists():
        print("目标文件不存在:", TARGET)
        return
    doc = toml.load(TARGET)
    if "workspace" not in doc or "dependencies" not in doc["workspace"]:
        print("文件中未找到 [workspace.dependencies]，退出")
        return
    ws_deps = doc["workspace"]["dependencies"]
    changes = process_dependencies_table(ws_deps)
    if not changes:
        print("未发现需要替换的 git -> local 依赖。")
        return
    print("发现以下替换：")
    for k, v in changes.items():
        if v is None:
            print(f"  {k} -> 本地包未找到，保留 git 依赖（请手动处理）")
        else:
            print(f"  {k} -> path = \"{v}\"")
    if dry_run:
        print("\n预览完成。若确认，使用 --apply 写回文件。")
    else:
        # 写回
        doc["workspace"]["dependencies"] = ws_deps
        with TARGET.open("w", encoding="utf-8") as f:
            toml.dump(doc, f)
        print("\n已写回", TARGET)

if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("--apply", action="store_true", help="写回修改到 Cargo.toml")
    args = p.parse_args()
    main(dry_run=not args.apply)