#!/bin/sh
# Print the wasi-sdk 25 directory to use when WASI_SDK_PATH is not set, or nothing if there is none.
# Order: what `spared setup` installs (~/.spared/wasi-sdk, or a directory inside it), then a
# download in /opt/wasi-sdk-dl, then /opt/wasi-sdk-*.
for d in "$HOME/.spared/wasi-sdk" "$HOME"/.spared/wasi-sdk/wasi-sdk-* /opt/wasi-sdk-dl/wasi-sdk-25.0-x86_64-linux /opt/wasi-sdk-*; do
  if [ -x "$d/bin/clang" ]; then
    printf '%s\n' "$d"
    exit 0
  fi
done
