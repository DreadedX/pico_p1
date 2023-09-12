#!/bin/bash

mkdir -p target/firmware
cargo objcopy --release -- -O binary target/firmware/firmware
shasum -a 512 -b target/firmware/firmware | dd ibs=128 count=1 | xxd -p -r > target/firmware/checksum
signify -S -m target/firmware/checksum -s ~/Projects/crypt/R0/private/keys/firmware/pico_p1.sec -x target/firmware/checksum.sig
tail -n1 target/firmware/checksum.sig | base64 -d -i | dd ibs=10 skip=1 > target/firmware/signed
cat target/firmware/signed > target/firmware/firmware+signed
cat target/firmware/firmware >> target/firmware/firmware+signed
