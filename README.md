# forge-plugin-graph


## Install with use_package

Add this to `~/.config/forge/init.rhai`:

```rhai
use_package("graph", #{
  vc: "https://github.com/martial-plains/forge-plugin-graph",
  commands: ["graph-global", "graph-local"],
  grant: #{ directories: ["~/Notes"] },
  config: #{ vault: "~/Notes" },
});
```

Change `vault` and the granted directory to match your notes folder. Forge asks
before running the package build command the first time.

## Building

This plugin is built against the Forge plugin SDK (`forge-plugin-sdk`),
taken from the Forge repository named in `Cargo.toml` (change the `git =`
URL if Forge lives elsewhere).

    rustup target add wasm32-wasip2
    cargo build --release --target wasm32-wasip2

Copy the resulting `.wasm` to `~/.config/forge/plugins/`.

## License

GPL-3.0-or-later. See the license file in this directory.
