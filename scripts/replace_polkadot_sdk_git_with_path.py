import toml, os, sys, shutil

CARGO_TOML = "Cargo.toml"
SDK_LOCAL_ROOT = "third_party/polkadot-sdk"  # 你实际存放 polkadot-sdk 子crate 的目录

def find_crate_dir(crate_name):
    # 遍历 SDK_LOCAL_ROOT 下所有 Cargo.toml，找到 name 匹配的目录
    for root, dirs, files in os.walk(SDK_LOCAL_ROOT):
        if "Cargo.toml" in files:
            t = toml.load(os.path.join(root, "Cargo.toml"))
            name = t.get("package", {}).get("name")
            if name == crate_name:
                return os.path.relpath(root, ".")
    return None

def process_deps(deps):
    changed = False
    for k, v in list(deps.items()):
        if isinstance(v, dict) and v.get("git", "").startswith("https://github.com/paritytech/polkadot-sdk"):
            crate_dir = find_crate_dir(k)
            if crate_dir:
                print(f"替换 {k} 为 path = {crate_dir}")
                new_v = dict(v)
                new_v.pop("git", None)
                new_v.pop("branch", None)
                new_v["path"] = crate_dir
                deps[k] = new_v
                changed = True
            else:
                print(f"警告: 未找到本地 crate {k}，请手动处理")
    return changed

def main():
    bak = CARGO_TOML + ".bak"
    shutil.copy2(CARGO_TOML, bak)
    print(f"已备份原 Cargo.toml 到 {bak}")

    data = toml.load(CARGO_TOML)
    # 处理常见依赖区块
    for sec in ["dependencies", "dev-dependencies", "build-dependencies", "workspace.dependencies"]:
        if sec in data:
            process_deps(data[sec])
    # 递归处理所有表
    def walk(tbl):
        for v in tbl.values():
            if isinstance(v, dict):
                process_deps(v)
                walk(v)
    walk(data)

    with open(CARGO_TOML, "w") as f:
        toml.dump(data, f)
    print("处理完成，请检查 Cargo.toml 并运行 cargo check")

if __name__ == "__main__":
    main()