import toml

SDK_CARGO = "third_party/polkadot-sdk/Cargo.toml"
MAIN_CARGO = "Cargo.toml"

sdk = toml.load(SDK_CARGO)
main = toml.load(MAIN_CARGO)

sdk_deps = sdk.get("workspace", {}).get("dependencies", {})
main_deps = main.get("workspace", {}).get("dependencies", {})

def patch_path(v):
    if isinstance(v, dict) and "path" in v:
        p = v["path"]
        if not p.startswith("third_party/polkadot-sdk/"):
            v = v.copy()
            v["path"] = "third_party/polkadot-sdk/" + p
    return v

missing = {}
for k, v in sdk_deps.items():
    if k not in main_deps:
        missing[k] = patch_path(v)

if not missing:
    print("没有缺失的依赖需要补充。")
    exit(0)

print("# 下面内容可直接粘贴到主 Cargo.toml 的 [workspace.dependencies] 段：\n")
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