bin := env("HOME") / ".local/bin"
skip := "LICENSE justfile tmp-rm.rs"

# Link every script into ~/.local/bin; refuse to replace a file that differs from the repo copy
install:
    #!/usr/bin/env bash
    set -eu
    for f in *; do
        [[ " {{skip}} " == *" $f "* ]] && continue
        dst="{{bin}}/$f"
        if [[ -e "$dst" && ! -L "$dst" ]] && ! cmp -s "$f" "$dst"; then
            echo "skip $dst: differs from $PWD/$f" >&2
            continue
        fi
        ln -sfn "$PWD/$f" "$dst"
    done

# Build the static tmp-rm binary and install it into ~/.local/bin
tmp-rm:
    env -u RUSTC_WRAPPER cargo +nightly -Zscript build --release --target x86_64-unknown-linux-musl --target-dir ~/.cache/tmp-rm-build --manifest-path tmp-rm.rs
    install -m 755 ~/.cache/tmp-rm-build/x86_64-unknown-linux-musl/release/tmp-rm {{bin}}/tmp-rm

# Run the tmp-rm tests
test:
    env -u RUSTC_WRAPPER cargo +nightly -Zscript test --target-dir ~/.cache/tmp-rm-build --manifest-path tmp-rm.rs

# Format the Rust and renk scripts
fmt:
    rustfmt +nightly --edition 2024 tmp-rm.rs
    for f in *.rk; do renk fmt "$f" -o "$f"; done
