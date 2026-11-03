#!/bin/sh
# Moved: use https://halfhand.org/install.sh
set -eu
curl --proto '=https' --tlsv1.2 -LsSf \
    https://github.com/halfhandorg/halfhand/releases/latest/download/halfhand-installer.sh | sh
