#!/bin/bash
set -e

SDK_CARGO="third_party/polkadot-sdk/Cargo.toml"
MAIN_CARGO="Cargo.toml"
TMP_MISSING="tmp/missing_workspace_deps.txt"

while true; do
  # 1. 捕获所有缺失的 workspace.dependencies
  cargo check 2>&1 | grep 'dependency\.' | grep 'was not found' | \
    sed -n 's/.*dependency\.//;s/ .*$//p' | sed 's/[^a-zA-Z0-9_\-].*$//' | sort | uniq > $TMP_MISSING || true

  if [ ! -s $TMP_MISSING ]; then
    echo "✅ 所有 workspace.dependencies 已补全"
    break
  fi

  echo -e "\n缺失依赖如下："
  cat $TMP_MISSING

  # 2. 直接打印完整定义
  python3 - <<EOF
import toml

SDK_CARGO = "third_party/polkadot-sdk/Cargo.toml"
MAIN_CARGO = "Cargo.toml"
TMP_MISSING = "tmp/missing_workspace_deps.txt"

sdk = toml.load(SDK_CARGO)
main = toml.load(MAIN_CARGO)

sdk_deps = sdk.get("workspace", {}).get("dependencies", {})
main_deps = main.get("workspace", {}).get("dependencies", {})

with open(TMP_MISSING) as f:
    missing_crates = [l.strip() for l in f if l.strip()]

missing = {}
for k in missing_crates:
    if k not in main_deps and k in sdk_deps:
        v = sdk_deps[k]
        # 自动加前缀
        if isinstance(v, dict) and "path" in v and v["path"].startswith("substrate/"):
            v = v.copy()
            v["path"] = "third_party/polkadot-sdk/" + v["path"]
        elif isinstance(v, dict) and "path" in v and v["path"].startswith("cumulus/"):
            v = v.copy()
            v["path"] = "third_party/polkadot-sdk/" + v["path"]
        elif isinstance(v, dict) and "path" in v and v["path"].startswith("polkadot/"):
            v = v.copy()
            v["path"] = "third_party/polkadot-sdk/" + v["path"]
        elif isinstance(v, dict) and "path" in v and v["path"].startswith("utils/"):
            v = v.copy()
            v["path"] = "third_party/polkadot-sdk/" + v["path"]
        missing[k] = v

if not missing:
    print("没有缺失的依赖需要补充。")
    exit(0)

print("\n# 直接粘贴到主 Cargo.toml 的 [workspace.dependencies] 段：\n")
for k, v in missing.items():
    if isinstance(v, dict):
        inner = []
        for kk, vv in v.items():
            if isinstance(vv, list):
                arr = ", ".join(f'"{item}"' for item in vv)
                inner.append(f'{kk} = [{arr}]')
            elif isinstance(vv, bool):
                inner.append(f'{kk} = {str(vv).lower()}')
            else:
                inner.append(f'{kk} = "{vv}"')
        print(f'{k} = {{ {", ".join(inner)} }}')
    else:
        print(f'{k} = "{v}"')
EOF

  echo -e "\n请将上面内容复制到主 Cargo.toml 的 [workspace.dependencies] 段，保存后回车继续..."
  read
done