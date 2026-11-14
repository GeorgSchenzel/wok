# everything runs in the flake's dev shell: just <recipe>

# unit tests and the client/daemon tests (fake jail, runs anywhere)
test:
    nix develop -c cargo test

# the jail's properties with bwrap, no display, no GPU work (on the host)
jail-test:
    nix develop -c cargo build
    nix develop -c bash tests/jail.sh

# the display and the GPU through the daemon (on the GPU machine)
gpu-test:
    nix develop -c cargo build
    nix develop -c bash tests/gpu.sh

lint:
    nix develop -c cargo clippy --all-targets
    nix develop -c cargo fmt --check

# --show against a private headless sway (pixman, no GPU), never the desktop's
show-test:
    nix develop -c cargo build
    nix develop -c bash tests/show.sh

# --show on the real desktop session: opens a window there (Georg's gate)
show-desktop:
    nix develop -c cargo build
    nix develop -c bash tests/desktop.sh
