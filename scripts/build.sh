#!/bin/sh
set -e
cd "$(dirname "$0")/.."
mkdir -p bin
swiftc -O -o bin/vmcore vmcore/main.swift
codesign --force --sign - --entitlements vmcore/vmcore.entitlements bin/vmcore
cp scripts/split-image.py bin/split-image.py
cp scripts/inputd.py bin/inputd.py
cargo build --release
cp target/release/vmagent bin/vmagent
codesign --force --sign - bin/vmagent
echo "built bin/vmagent and bin/vmcore"
