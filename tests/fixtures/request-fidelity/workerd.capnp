using Workerd = import "/workerd/workerd.capnp";

# Direct ingress avoids Wrangler's development proxy URL normalization.
const config :Workerd.Config = (
  services = [(name = "fixture", worker = (
    compatibilityDate = "2026-04-01",
    modules = [
      (name = "index.js", esModule = embed "build/cloudflare/index.js"),
      (name = "index_bg.wasm", wasm = embed "build/cloudflare/index_bg.wasm")
    ]
  ))],
  sockets = [(name = "http", address = "127.0.0.1:8080", http = (), service = "fixture")]
);
