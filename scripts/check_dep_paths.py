# filepath: scripts/check_dep_paths.py
import os, re, sys

SDK_ROOT = "third_party/polkadot-sdk"
dep_file = "tmp/dep.list"

# 读取 dep.list
with open(dep_file) as f:
    lines = [l.strip() for l in f if l.strip() and not l.startswith("#")]

# 解析 crate 名
dep_re = re.compile(r'^([a-zA-Z0-9_\-]+)\s*=')
crate_names = []
for l in lines:
    m = dep_re.match(l)
    if m:
        crate_names.append(m.group(1))

# 查找实际路径
found = {}
for root, dirs, files in os.walk(SDK_ROOT):
    if "Cargo.toml" in files:
        with open(os.path.join(root, "Cargo.toml")) as f:
            for line in f:
                if line.strip().startswith("name = "):
                    name = line.split("=",1)[1].strip().strip('"')
                    found[name] = os.path.relpath(root, ".")
                    break

# 检查 dep.list 里的路径
for l in lines:
    m = dep_re.match(l)
    if not m:
        print(f"格式错误: {l}")
        continue
    name = m.group(1)
    expect = found.get(name)
    if not expect:
        print(f"未找到 {name} 的实际路径，请检查是否已拷贝到 third_party/polkadot-sdk")
        continue
    # 检查 path 是否一致
    if f'path = "{expect}"' in l:
        print(f"✔ {name} 路径正确: {expect}")
    else:
        print(f"✗ {name} 路径应为: {expect}，当前为: {l}")

print("\n建议：用上面实际路径修正 dep.list 和 Cargo.toml")