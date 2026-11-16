#!/bin/bash
cargo build --locked --release --target $INPUT_TARGET --features $INPUT_FEATURES
