python scripts/ci/release.py stamp "$env:VERSION"
if ($LASTEXITCODE) { exit $LASTEXITCODE }
cargo test --locked -p asagiri-win
if ($LASTEXITCODE) { exit $LASTEXITCODE }
