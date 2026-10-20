docker stop my-sunchain-node-evm
docker rm my-sunchain-node-evm
sudo rm -rf ./temp/
./build.sh
docker run -d   -e EXTRA_ARGS="--bootnodes /ip4/192.168.1.98/tcp/30333/p2p/12D3KooWP2fwEz8p7uhd9hdoExngsFaQCXk1vFmAjayiaciPkhRR --chain ./evm_sunchain.json" --name my-sunchain-node-evm   -p 16888:16888/tcp   -p 16888:16888/udp   -p 1688:1688/tcp  -v "$(pwd)/temp":/root/.codigger/.sunchain/chaindata/default_base sunchain:evm-v1.0.0
docker logs -f --tail 100 my-sunchain-node-evm