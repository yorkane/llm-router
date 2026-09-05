#!/usr/bin/env bash
set -e
source .venv/bin/activate
maturin develop --release 
pytest "$@"
