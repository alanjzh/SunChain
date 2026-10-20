import toml, re

umbrella = toml.load("third_party/polkadot-sdk/umbrella/Cargo.toml")

# 收集 crate->path 映射
crate2path = {}
for k, v in umbrella.items():
    if k.startswith("dependencies."):
        name = k.split(".", 1)[1]
        path = v.get("path")
        if path:
            # 统一加前缀
            crate2path[name] = "third_party/polkadot-sdk/" + path.lstrip("./").lstrip("../")

# 处理 dep.list
with open("tmp/dep.list") as f:
    lines = f.readlines()

for line in lines:
    m = re.match(r'^([a-zA-Z0-9_\-]+)\s*=\s*\{[^}]*path\s*=\s*"([^"]+)"', line)
    if m:
        crate, old_path = m.group(1), m.group(2)
        new_path = crate2path.get(crate)
        if new_path:
            # 保留其它字段
            newline = re.sub(r'path\s*=\s*"[^"]+"', f'path = "{new_path}"', line)
            print(newline, end="")
        else:
            print(f"# 未在 umbrella Cargo.toml 找到 {crate}，请手动检查", end="")
            print(line, end="")
    else:
        print(line, end="")