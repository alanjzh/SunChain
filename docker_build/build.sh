#! /bin/bash -e

cp ../target/release/sunchain .

docker build . -t sunchain:evm-v1.0.0 -f Dockerfile-Sunchain

rm sunchain