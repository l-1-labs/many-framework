nix run github:cargo2nix/cargo2nix --max-jobs $CPUCORES -- -f docker/e2e/Cargo.nix.new
mv docker/e2e/Cargo.nix.new docker/e2e/Cargo.nix
chown $UINFO docker/e2e/Cargo.nix
