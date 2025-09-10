use http::StatusCode;
use kode_bridge::{ipc_http_server::HttpResponse, IpcHttpServer, Result, Router};

#[tokio::main]
async fn main() -> Result<()> {
    let router = Router::new()
        .get("/version", |_| async move {
            Ok(HttpResponse::builder().status(StatusCode::OK).text("996").build())
        });

    let mut server = IpcHttpServer::new("/tmp/example.sock")?
        .router(router);
    
    println!("🚀 Server listening on /tmp/example.sock");
    server.serve().await
}