# Slint UI prototype

Run this isolated preview from the Rust workspace root:

```powershell
cargo run --release --manifest-path tools/usage-ui-prototype/Cargo.toml --target-dir target
```

The window and notification-area icon exercise Slint's Windows host integration.
All provider names, quotas, balances, and reset times are illustrative. The
prototype does not read accounts, call providers, or modify the backend. Its
own Cargo workspace and lockfile keep this trial separate from the provider
workspace until the toolkit is accepted.
