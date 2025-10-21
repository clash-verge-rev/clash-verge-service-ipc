use crate::IPC_AUTH_EXPECT;
use kode_bridge::errors::KodeBridgeError;
use kode_bridge::ipc_http_server::RequestContext;

#[derive(Debug, PartialEq, Eq)]
pub enum AuthStatus {
    Authorized,
}

pub fn ipc_request_context_to_auth_context(
    ctx: &RequestContext,
) -> Result<AuthStatus, KodeBridgeError> {
    let headers = &ctx.headers;
    match headers.get("X-IPC-Magic") {
        Some(token) if token == IPC_AUTH_EXPECT => Ok(AuthStatus::Authorized),
        Some(_) => Err(KodeBridgeError::ClientError { status: 401 }),
        None => Err(KodeBridgeError::ClientError { status: 401 }),
    }
}
