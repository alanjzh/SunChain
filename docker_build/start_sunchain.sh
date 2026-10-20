#!/bin/bash
# start-sunchain.sh

# 如果任何命令失败，则退出脚本
set -e

# 定义数据目录
DATA_DIR="/root/.codigger/.sunchain/chaindata/default_base"

# 确保数据目录存在
echo "Ensuring data directory exists at $DATA_DIR"
mkdir -p "$DATA_DIR"

# 定义网络密钥的路径
KEY_PATH="$DATA_DIR/chains/SUN/network/secret_ed25519"

# 检查网络密钥是否存在
if [ ! -f "$KEY_PATH" ]; then
    echo "Network key not found at $KEY_PATH. Generating a new node key..."
    # 生成新的节点密钥
    /sunchain key generate-node-key --chain ./evm_sunchain.json --base-path "$DATA_DIR"
else
    echo "Existing node key found at $KEY_PATH. Using it."
fi

# 启动 sunchain 节点
# 将所有传入的参数传递给 sunchain 命令
echo "Starting sunchain node..."
exec /sunchain  -d "$DATA_DIR" --port 16888 "$@" ${EXTRA_ARGS}