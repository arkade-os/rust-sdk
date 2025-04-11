use anyhow::Result;
use ark_rest::apis::{ark_service_api::ArkServiceApi, configuration::Configuration};
use axum::{
    extract::Path,
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/info", get(get_info))
        .route("/events", get(get_events))
        .route("/vtxos/:address", get(list_vtxos))
        .route("/round/:txid", get(get_round))
        .route("/round/id/:id", get(get_round_by_id));

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Server starting on http://{}", addr);

    axum::serve(
        tokio::net::TcpListener::bind(addr).await?,
        app.into_make_service(),
    )
    .await?;

    Ok(())
}

async fn get_info() -> Json<String> {
    let client = create_client();
    let info = client
        .get_info()
        .await
        .unwrap_or_else(|e| format!("Error: {}", e));
    Json(info)
}

async fn get_events() -> Json<String> {
    let client = create_client();
    let events = client
        .get_event_stream()
        .await
        .unwrap_or_else(|e| format!("Error: {}", e));
    Json(events)
}

async fn list_vtxos(Path(address): Path<String>) -> Json<String> {
    let client = create_client();
    let vtxos = client
        .list_vtxos(&address)
        .await
        .unwrap_or_else(|e| format!("Error: {}", e));
    Json(vtxos)
}

async fn get_round(Path(txid): Path<String>) -> Json<String> {
    let client = create_client();
    let round = client
        .get_round(&txid)
        .await
        .unwrap_or_else(|e| format!("Error: {}", e));
    Json(round)
}

async fn get_round_by_id(Path(id): Path<String>) -> Json<String> {
    let client = create_client();
    let round = client
        .get_round_by_id(&id)
        .await
        .unwrap_or_else(|e| format!("Error: {}", e));
    Json(round)
}

fn create_client() -> ArkServiceApi {
    let config = Configuration {
        base_path: "http://insiders.signet.arklabs.to".to_string(),
        ..Default::default()
    };
    ArkServiceApi::new(config)
}

async fn index() -> Html<String> {
    Html(format!(
        r#"
        <!DOCTYPE html>
        <html>
            <head>
                <title>Ark Web App</title>
                <style>
                    body {{ font-family: Arial, sans-serif; margin: 2em; }}
                    pre {{ background: #f5f5f5; padding: 1em; }}
                    .endpoint {{ margin: 1em 0; padding: 1em; border: 1px solid #ddd; }}
                </style>
            </head>
            <body>
                <h1>Ark API Explorer</h1>
                
                <div class="endpoint">
                    <h2>Server Info</h2>
                    <button onclick="fetchEndpoint('/info')">Get Info</button>
                </div>

                <div class="endpoint">
                    <h2>Event Stream</h2>
                    <button onclick="fetchEndpoint('/events')">Get Events</button>
                </div>

                <div class="endpoint">
                    <h2>List VTXOs</h2>
                    <input id="address" placeholder="Enter address">
                    <button onclick="fetchVtxos()">Get VTXOs</button>
                </div>

                <div class="endpoint">
                    <h2>Get Round</h2>
                    <input id="txid" placeholder="Enter transaction ID">
                    <button onclick="fetchRound()">Get Round</button>
                </div>

                <div class="endpoint">
                    <h2>Get Round by ID</h2>
                    <input id="roundId" placeholder="Enter round ID">
                    <button onclick="fetchRoundById()">Get Round</button>
                </div>

                <pre id="result">Results will appear here...</pre>

                <script>
                    async function fetchEndpoint(endpoint) {{
                        try {{
                            const response = await fetch(endpoint);
                            const data = await response.json();
                            document.getElementById('result').textContent = 
                                JSON.stringify(data, null, 2);
                        }} catch (error) {{
                            document.getElementById('result').textContent = 
                                `Error: ${error.message}`;
                        }}
                    }}

                    async function fetchVtxos() {{
                        const address = document.getElementById('address').value;
                        await fetchEndpoint(`/vtxos/${address}`);
                    }}

                    async function fetchRound() {{
                        const txid = document.getElementById('txid').value;
                        await fetchEndpoint(`/round/${txid}`);
                    }}

                    async function fetchRoundById() {{
                        const id = document.getElementById('roundId').value;
                        await fetchEndpoint(`/round/id/${id}`);
                    }}
                </script>
            </body>
        </html>
        "#
    ))
}
