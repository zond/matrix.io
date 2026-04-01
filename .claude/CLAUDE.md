# matrix.io

Paper.io-like multiplayer territory game built with Rust.

## Architecture

- **Cargo workspace** with 3 crates: `shared/`, `server/`, `client/`
- **Shared**: game types + Cap'n Proto schema (`schema/protocol.capnp`) with encode/decode helpers
- **Server**: axum WebSocket server (port 3000), serves static files from `web/`, 20Hz game loop
- **Client**: Rust WASM (wasm-bindgen), canvas rendering, binary WebSocket, client-side position extrapolation

## Build & Run

```bash
# Build WASM client
wasm-pack build client --target web --out-dir ../web/pkg

# Run server (also serves web/)
cargo run -p server
# Open http://localhost:3000
```

## Serialization

Cap'n Proto for all wire messages (binary WebSocket frames). Schema in `shared/schema/protocol.capnp`, codegen via `shared/build.rs`.

## Key Design Decisions

- No trunk — use wasm-pack + static HTML
- Channel-based game loop (no Arc<Mutex> on game state)
- Territory capture via BFS flood-fill
- Server filters broadcasts to visibility radius (40 cells)
