//! The friends service: an external HTTP service (outside fluxolap, e.g. the
//! layer that also handles authentication) that says which nodes a node may
//! talk to. The API is documented in `docs/mesh.md`; [`router`] is a minimal
//! implementation of it for tests and small setups.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Friend {
    pub id: String,
    /// `host:port` of the friend's mesh MQTT listener.
    pub mqtt: String,
    /// Base URL of the friend's REST API.
    pub api: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FriendsResponse {
    pub friends: Vec<Friend>,
}

/// Asks the service for the friends of `node`. `token`, if set, is sent as
/// a bearer token.
pub async fn fetch(client: &reqwest::Client, url: &str, node: &str, token: Option<&str>) -> Result<Vec<Friend>, reqwest::Error> {
    let mut request = client.get(url).query(&[("node", node)]);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    Ok(request.send().await?.error_for_status()?.json::<FriendsResponse>().await?.friends)
}

#[derive(Debug, Deserialize)]
struct NodeQuery {
    node: String,
}

/// A minimal friends service: every node in `nodes` is a friend of every
/// other. Serves `GET /friends?node=<id>`.
pub fn router(nodes: Vec<Friend>) -> Router {
    async fn friends(State(nodes): State<Arc<Vec<Friend>>>, Query(q): Query<NodeQuery>) -> Json<FriendsResponse> {
        Json(FriendsResponse { friends: nodes.iter().filter(|n| n.id != q.node).cloned().collect() })
    }
    Router::new().route("/friends", get(friends)).with_state(Arc::new(nodes))
}
