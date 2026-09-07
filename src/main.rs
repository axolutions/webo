use std::sync::Arc;
use tokio::sync::RwLock;
use webo::{collector, github, metrics::State, projects, server, store::Store};

#[tokio::main]
async fn main() {
    let bind = std::env::var("WEBO_BIND").unwrap_or_else(|_| "0.0.0.0:5050".into());
    let sample_secs: u64 = std::env::var("WEBO_SAMPLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(15);
    // 24 h of history
    let cap = (24 * 3600 / sample_secs.max(1)) as usize;

    let db_path = std::env::var("WEBO_DB_PATH").unwrap_or_else(|_| "webo.db".into());
    let store = Arc::new(Store::open(std::path::Path::new(&db_path)).expect("open store"));

    let state: server::Shared = Arc::new(RwLock::new(State::new(cap)));
    tokio::spawn(collector::run(state.clone(), sample_secs));
    tokio::spawn(projects::run(state.clone(), store.clone(), sample_secs));
    tokio::spawn(github::run(store.clone(), 120));
    tokio::spawn(webo::logs::run(store.clone(), 10));
    // 5-minute aggregates: the 7-day charts survive webo's own deploys
    tokio::spawn(webo::persist::run(state.clone(), store.clone(), 300));
    // hourly check, daily dump per postgres database, 7 kept
    tokio::spawn(webo::backups::run(store.clone(), 3600));

    // Login is on when the Clerk keys are there. Half a configuration is a
    // mistake, not a mode: refusing to start beats serving the panel open
    // while looking configured.
    let auth = match webo::auth::Auth::from_env() {
        Ok(a) => a.map(Arc::new),
        Err(e) => {
            eprintln!("webo: {e}");
            std::process::exit(1);
        }
    };
    match &auth {
        Some(_) => println!("webo auth: Clerk (the panel and /mcp need a credential)"),
        None => println!("webo auth: none — local mode, everything is open"),
    }

    let api = server::Api { state, store, auth };

    // MCP: operational surface, so it binds ONLY to the Tailscale address.
    // Without one it does not start — publishing these tools to the network
    // would be worse than not having them.
    let mcp_port: u16 = std::env::var("WEBO_MCP_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5051);
    match webo::net::mcp_bind(mcp_port, &webo::net::local_addrs()) {
        Some(addr) => {
            let mcp_api = api.clone();
            match tokio::net::TcpListener::bind(&addr).await {
                Ok(l) => {
                    println!("webo mcp serving at http://{addr}/mcp (tailnet only)");
                    tokio::spawn(async move {
                        let _ = axum::serve(l, webo::mcp::app(mcp_api)).await;
                    });
                }
                Err(e) => eprintln!("webo mcp: could not bind {addr}: {e}"),
            }
        }
        None => println!(
            "webo mcp: no Tailscale address found — MCP not started \
             (set WEBO_MCP_BIND to override)"
        ),
    }

    let listener = tokio::net::TcpListener::bind(&bind).await.expect("bind");
    println!("webo serving at http://{bind}");
    axum::serve(listener, server::app(api)).await.expect("serve");
}
