//! The fluxolap REST API, from the browser (fetch via gloo-net), typed
//! with the server's own request/response types.

use fluxolap_api_types::{CellDto, CellList, CellQuery, CubeDto, CubeList, DatabaseDto, DatabaseList, DimensionDto, DimensionList, ElementDto, ElementList, ExampleDto, ExampleList, Problem, Ref, ServerDto};
use gloo_net::http::{Request, Response};
use serde::de::DeserializeOwned;

/// Where the API is. An empty `base` means this page's own origin: the
/// case behind `trunk serve`'s proxy and when fluxolap serves the app
/// itself at `/ui`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Api {
    pub base: String,
}

pub type Result<T> = std::result::Result<T, String>;

impl Api {
    pub fn new(base: &str) -> Self {
        let base = base.trim().trim_end_matches('/');
        let base = base.strip_suffix("/api/v1").unwrap_or(base);
        let base = if base.is_empty() || base.contains("://") { base.to_string() } else { format!("http://{base}") };
        Self { base }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1{path}", self.base)
    }

    async fn read<T: DeserializeOwned>(response: std::result::Result<Response, gloo_net::Error>) -> Result<T> {
        let response = response.map_err(|e| format!("can't reach the server ({e})"))?;
        if !response.ok() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(serde_json::from_str::<Problem>(&body).map(|p| p.detail).unwrap_or_else(|_| format!("HTTP {status}")));
        }
        response.json::<T>().await.map_err(|e| format!("unexpected response: {e}"))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Self::read(Request::get(&self.url(path)).send().await).await
    }

    pub async fn server(&self) -> Result<ServerDto> {
        self.get("").await
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

    /// A dimension's elements; `roots_only` for just its top-level ones.
    pub async fn elements(&self, db: u32, dim: u32, roots_only: bool) -> Result<Vec<ElementDto>> {
        let query = if roots_only { "?roots=true" } else { "" };
        Ok(self.get::<ElementList>(&format!("/databases/{db}/dimensions/{dim}/elements{query}")).await?.items)
    }

    /// The example databases the server offers to import.
    pub async fn examples(&self) -> Result<Vec<ExampleDto>> {
        Ok(self.get::<ExampleList>("/examples").await?.items)
    }

    /// Has the server import one of its example databases.
    pub async fn import_example(&self, file: &str) -> Result<DatabaseDto> {
        Self::read(Request::post(&self.url(&format!("/examples/{}/import", encode(file)))).send().await).await
    }

    /// Uploads a database file (`.bin.zst`) from this computer.
    pub async fn import_file(&self, file: web_sys::File) -> Result<DatabaseDto> {
        let request = Request::post(&self.url("/databases/import")).header("content-type", "application/octet-stream").body(file).map_err(|e| e.to_string())?;
        Self::read(request.send().await).await
    }

    /// One element-id list per cube dimension, read as their cartesian
    /// product; `skip_empty` leaves out the cells without a value.
    pub async fn area(&self, db: u32, cube: u32, area: Vec<Vec<u32>>, skip_empty: bool) -> Result<Vec<CellDto>> {
        let query = CellQuery { paths: None, area: Some(area.into_iter().map(|ids| ids.into_iter().map(Ref::Id).collect()).collect()), skip_empty };
        let request = Request::post(&self.url(&format!("/databases/{db}/cubes/{cube}/cells/query"))).json(&query).map_err(|e| e.to_string())?;
        Ok(Self::read::<CellList>(request.send().await).await?.items)
    }
}

/// Percent-encodes `s` for use as one URL path segment.
fn encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments_are_encoded() {
        assert_eq!(encode("db-3-Climate.bin.zst"), "db-3-Climate.bin.zst");
        assert_eq!(encode("a b/ü.bin.zst"), "a%20b%2F%C3%BC.bin.zst");
    }
}
