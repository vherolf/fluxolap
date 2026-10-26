//! A typed client for the fluxolap REST API, using the server's own
//! request/response types (`fluxolap-api-types`).

use fluxolap_api_types::{
    CellDto, CellList, CellQuery, CellValueWrite, CubeDto, CubeList, CubeUpdate, DatabaseDto, DatabaseList, DatabaseUpdate, DimensionDto, DimensionList,
    DimensionUpdate, ElementDto, ElementList, Problem, Ref, ServerDto, ServerStats, Value,
};
use reqwest::{Method, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// A failed request, ready to show: the server's problem detail when it
/// sent one, otherwise what went wrong on the way.
#[derive(Debug, Clone, PartialEq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    /// e.g. `http://localhost:7777/api/v1`
    base: String,
}

impl Client {
    /// `server` is the server's address, e.g. `http://localhost:7777`
    /// (a trailing `/api/v1` is accepted too).
    pub fn new(server: &str) -> Self {
        let server = server.trim_end_matches('/');
        let server = server.strip_suffix("/api/v1").unwrap_or(server);
        let server = if server.contains("://") { server.to_string() } else { format!("http://{server}") };
        Self { http: reqwest::Client::new(), base: format!("{server}/api/v1") }
    }

    /// The server address this client talks to, without `/api/v1`.
    pub fn server_url(&self) -> &str {
        self.base.strip_suffix("/api/v1").unwrap_or(&self.base)
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http.request(method, format!("{}{path}", self.base))
    }

    async fn send(request: RequestBuilder) -> Result<Response> {
        Self::check(request.send().await.map_err(|e| Error(connection_error(&e)))?).await
    }

    /// A successful response, or the error its problem details describe.
    async fn check(response: Response) -> Result<Response> {
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(Error(match serde_json::from_str::<Problem>(&body) {
            Ok(p) => p.detail,
            Err(_) => format!("{status}"),
        }))
    }

    async fn json<T: DeserializeOwned>(request: RequestBuilder) -> Result<T> {
        Self::send(request).await?.json().await.map_err(|e| Error(format!("unexpected response: {e}")))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Self::json(self.request(Method::GET, path)).await
    }

    async fn with_body<B: Serialize, T: DeserializeOwned>(&self, method: Method, path: &str, body: &B) -> Result<T> {
        Self::json(self.request(method, path).json(body)).await
    }

    async fn delete(&self, path: &str) -> Result<()> {
        Self::send(self.request(Method::DELETE, path)).await.map(|_| ())
    }

    pub async fn server(&self) -> Result<ServerDto> {
        self.get("").await
    }

    /// The server's memory and statistics; `None` from a server too old to
    /// have them (before fluxolap 21).
    pub async fn stats(&self) -> Result<Option<ServerStats>> {
        let response = self.request(Method::GET, "/stats").send().await.map_err(|e| Error(connection_error(&e)))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Self::check(response).await?.json().await.map(Some).map_err(|e| Error(format!("unexpected response: {e}")))
    }

    pub async fn databases(&self) -> Result<Vec<DatabaseDto>> {
        Ok(self.get::<DatabaseList>("/databases").await?.items)
    }

    pub async fn dimensions(&self, db: u32) -> Result<Vec<DimensionDto>> {
        Ok(self.get::<DimensionList>(&format!("/databases/{db}/dimensions")).await?.items)
    }

    pub async fn cubes(&self, db: u32) -> Result<Vec<CubeDto>> {
        Ok(self.get::<CubeList>(&format!("/databases/{db}/cubes")).await?.items)
    }

    /// A dimension's elements; with `roots_only`, just the top-level ones
    /// (cheap, for a dimension with many elements that's only a filter).
    pub async fn elements(&self, db: u32, dim: u32, roots_only: bool) -> Result<Vec<ElementDto>> {
        let query = if roots_only { "?roots=true" } else { "" };
        Ok(self.get::<ElementList>(&format!("/databases/{db}/dimensions/{dim}/elements{query}")).await?.items)
    }

    /// Reads an area: one list of element ids per cube dimension.
    pub async fn area(&self, db: u32, cube: u32, area: Vec<Vec<u32>>) -> Result<Vec<CellDto>> {
        let query = CellQuery { paths: None, area: Some(area.into_iter().map(|ids| ids.into_iter().map(Ref::Id).collect()).collect()), skip_empty: false };
        Ok(self.with_body::<_, CellList>(Method::POST, &format!("/databases/{db}/cubes/{cube}/cells/query"), &query).await?.items)
    }

    /// Writes one cell (`None` clears it) and returns its new value.
    pub async fn write_cell(&self, db: u32, cube: u32, path: &[u32], value: Option<Value>) -> Result<CellDto> {
        let path: Vec<String> = path.iter().map(u32::to_string).collect();
        self.with_body(Method::PUT, &format!("/databases/{db}/cubes/{cube}/cells/{}", path.join(",")), &CellValueWrite { value }).await
    }

    pub async fn clear_cube(&self, db: u32, cube: u32) -> Result<()> {
        self.delete(&format!("/databases/{db}/cubes/{cube}/cells")).await
    }

    pub async fn delete_database(&self, db: u32) -> Result<()> {
        self.delete(&format!("/databases/{db}")).await
    }

    pub async fn delete_dimension(&self, db: u32, dim: u32) -> Result<()> {
        self.delete(&format!("/databases/{db}/dimensions/{dim}")).await
    }

    pub async fn delete_cube(&self, db: u32, cube: u32) -> Result<()> {
        self.delete(&format!("/databases/{db}/cubes/{cube}")).await
    }

    pub async fn rename_database(&self, db: u32, name: &str) -> Result<DatabaseDto> {
        self.with_body(Method::PATCH, &format!("/databases/{db}"), &DatabaseUpdate { name: Some(name.to_string()) }).await
    }

    pub async fn rename_dimension(&self, db: u32, dim: u32, name: &str) -> Result<DimensionDto> {
        self.with_body(Method::PATCH, &format!("/databases/{db}/dimensions/{dim}"), &DimensionUpdate { name: Some(name.to_string()) }).await
    }

    pub async fn rename_cube(&self, db: u32, cube: u32, name: &str) -> Result<CubeDto> {
        self.with_body(Method::PATCH, &format!("/databases/{db}/cubes/{cube}"), &CubeUpdate { name: Some(name.to_string()) }).await
    }

    /// Uploads a database file (`.bin.zst`).
    pub async fn import(&self, file: Vec<u8>) -> Result<DatabaseDto> {
        Self::json(self.request(Method::POST, "/databases/import").header("content-type", "application/octet-stream").body(file)).await
    }

    /// Downloads a database as a file's bytes.
    pub async fn export(&self, db: u32) -> Result<Vec<u8>> {
        let response = Self::send(self.request(Method::GET, &format!("/databases/{db}/export"))).await?;
        Ok(response.bytes().await.map_err(|e| Error(connection_error(&e)))?.to_vec())
    }
}

fn connection_error(e: &reqwest::Error) -> String {
    if e.is_connect() {
        "can't connect to the server".to_string()
    } else if e.is_timeout() {
        "the server didn't answer in time".to_string()
    } else {
        e.to_string()
    }
}
