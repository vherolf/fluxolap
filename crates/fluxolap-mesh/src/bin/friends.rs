//! `fluxolap-friends <nodes.json> [port]`: the minimal friends service.
//! `nodes.json` is `[{"id": "...", "mqtt": "host:port", "api": "http://..."}]`;
//! all listed nodes are friends of each other. Port defaults to 7780.

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let file = args.next().unwrap_or_else(|| usage());
    let port: u16 = args.next().map_or(7780, |p| p.parse().unwrap_or_else(|_| usage()));
    let nodes = serde_json::from_str(&std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("can't read {file}: {e}")))
        .unwrap_or_else(|e| panic!("{file} is not a list of nodes: {e}"));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await.unwrap_or_else(|e| panic!("failed to bind port {port}: {e}"));
    eprintln!("friends service on port {port}: GET /friends?node=<id>");
    axum::serve(listener, fluxolap_mesh::friends::router(nodes)).await.expect("server error");
}

fn usage() -> ! {
    eprintln!("usage: fluxolap-friends <nodes.json> [port]");
    std::process::exit(2)
}
