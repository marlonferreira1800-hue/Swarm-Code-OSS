#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4000);
    let project_root = swarm_server::resolve_project_root();

    swarm_server::run_server(port, project_root)
        .await
        .expect("erro no servidor swarm_server");
}
