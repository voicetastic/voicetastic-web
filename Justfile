default: build

# Source emscripten before any wasm-pack run so the AMR-NB build script
# (opencore-amrnb-src) can find `emcc` in PATH. Idempotent — the script
# just sets EMSDK / PATH for the current command.
emsdk := "source /etc/profile.d/emscripten.sh"

# Development build — fast incremental, large wasm with debug info.
build:
    {{emsdk}} && wasm-pack build --target web --out-dir pkg --dev

# Production build — `--release` profile, no debug info, slim wasm.
build-release:
    {{emsdk}} && wasm-pack build --target web --out-dir pkg --release

# Optional: post-process with `wasm-opt -Oz` for the smallest possible
# artifact (cuts ~30–40 % further). Requires `binaryen` (`pacman -S binaryen`).
build-min: build-release
    wasm-opt -Oz pkg/voicetastic_web_bg.wasm -o pkg/voicetastic_web_bg.wasm
    @echo "minified: $(ls -lh pkg/voicetastic_web_bg.wasm | awk '{print $5}')"

# Sanity-check the built wasm has no unresolved `env.*` imports — the class
# of failure that bit us with `instant 0.1` and again with raw `Instant`.
check-env:
    @node --input-type=module -e " \
      import('fs').then(async fs => { \
        const buf = fs.readFileSync('pkg/voicetastic_web_bg.wasm'); \
        const mod = await WebAssembly.compile(buf); \
        const env = WebAssembly.Module.imports(mod).filter(i => i.module === 'env'); \
        console.log(env.length === 0 ? '✅ no env imports' : '❌ env: ' + JSON.stringify(env)); \
        process.exit(env.length === 0 ? 0 : 1); \
      });"

# Serve the harness over localhost (Web Serial + WASM need a secure context
# — localhost qualifies, file:// doesn't). Then open http://localhost:8080.
serve:
    python3 -m http.server 8080

# Clean Rust + the generated pkg dir.
clean:
    cargo clean
    rm -rf pkg
