//! End-to-end tests over real HTTP against `fluxolap_api::router`.

use reqwest::{Client, Response, StatusCode};
use serde_json::{json, Value};

struct Api {
    base: String,
    client: Client,
}

impl Api {
    async fn spawn() -> Self {
        let app = fluxolap_api::router(fluxolap_engine::AppState::new());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { base: format!("http://{addr}/api/v1"), client: Client::new() }
    }

    async fn send(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Response {
        let mut req = self.client.request(method, format!("{}{path}", self.base));
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await.unwrap()
    }

    async fn get(&self, path: &str) -> Response {
        self.send(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> Response {
        self.send(reqwest::Method::POST, path, Some(body)).await
    }
    async fn put(&self, path: &str, body: Value) -> Response {
        self.send(reqwest::Method::PUT, path, Some(body)).await
    }
    async fn patch(&self, path: &str, body: Value) -> Response {
        self.send(reqwest::Method::PATCH, path, Some(body)).await
    }
    async fn delete(&self, path: &str) -> Response {
        self.send(reqwest::Method::DELETE, path, None).await
    }

    /// Creates database `Sales` with a `Region` dimension (North, South,
    /// Total = North + 2*South) and a `Month` dimension (Jan, Feb,
    /// Q1 = Jan + Feb), and a cube `Revenue` over both. Element ids follow
    /// creation order per dimension: Total 0, North 1, South 2; Jan 0,
    /// Feb 1, Q1 2. Returns the `Region` dimension's and the cube's URL
    /// paths.
    async fn fixture(&self) -> (String, String) {
        assert_eq!(self.post("/databases", json!({"name": "Sales"})).await.status(), StatusCode::CREATED);
        let region = self.post("/databases/0/dimensions", json!({"name": "Region"})).await.json::<Value>().await.unwrap()["id"].clone();
        let month = self.post("/databases/0/dimensions", json!({"name": "Month"})).await.json::<Value>().await.unwrap()["id"].clone();
        let r = self
            .post(
                &format!("/databases/0/dimensions/{region}/elements"),
                json!({"items": [
                    {"name": "Total", "children": [{"element": "North"}, {"element": "South", "weight": 2}]},
                    {"name": "North"},
                    {"name": "South"},
                ]}),
            )
            .await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let r = self
            .post(
                &format!("/databases/0/dimensions/{month}/elements"),
                json!({"items": [{"name": "Jan"}, {"name": "Feb"}, {"name": "Q1", "children": [{"element": "Jan"}, {"element": "Feb"}]}]}),
            )
            .await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let cube = self.post("/databases/0/cubes", json!({"name": "Revenue", "dimensions": ["Region", "Month"]})).await;
        assert_eq!(cube.status(), StatusCode::CREATED);
        (format!("/databases/0/dimensions/{region}"), format!("/databases/0/cubes/{}", cube.json::<Value>().await.unwrap()["id"]))
    }
}

async fn problem(r: Response) -> Value {
    assert_eq!(r.headers()["content-type"], "application/problem+json");
    r.json().await.unwrap()
}

#[tokio::test]
async fn database_crud_follows_rest_conventions() {
    let api = Api::spawn().await;

    let r = api.post("/databases", json!({"name": "Sales"})).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.headers()["location"], "/api/v1/databases/0");
    assert_eq!(r.json::<Value>().await.unwrap(), json!({"id": 0, "name": "Sales", "type": "normal"}));

    let list: Value = api.get("/databases?name=Sales").await.json().await.unwrap();
    assert_eq!(list["total"], 1);

    let r = api.post("/databases", json!({"name": "Sales"})).await;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let p = problem(r).await;
    assert_eq!(p["code"], "database_name_in_use");
    assert_eq!(p["status"], 409);

    let r = api.patch("/databases/0", json!({"name": "Revenue"})).await;
    assert_eq!(r.json::<Value>().await.unwrap()["name"], "Revenue");

    assert_eq!(api.delete("/databases/0").await.status(), StatusCode::NO_CONTENT);
    let r = api.get("/databases/0").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(problem(r).await["code"], "database_not_found");
}

#[tokio::test]
async fn malformed_requests_get_problem_details() {
    let api = Api::spawn().await;

    let r = api.client.post(format!("{}/databases", api.base)).header("content-type", "application/json").body("{").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem(r).await["code"], "invalid_body");

    let r = api.post("/databases", json!({"name": "X", "colour": "red"})).await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem(r).await["code"], "invalid_body");

    let r = api.get("/databases/abc").await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(problem(r).await["code"], "invalid_path");

    let r = api.get("/nothing/here").await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(problem(r).await["code"], "route_not_found");
}

#[tokio::test]
async fn element_batches_resolve_names_within_the_batch_and_are_atomic() {
    let api = Api::spawn().await;
    let (region, _) = api.fixture().await;
    let elements = &format!("{region}/elements");

    let list: Value = api.get(elements).await.json().await.unwrap();
    assert_eq!(list["total"], 3);
    let total = &list["items"][0];
    assert_eq!(total["name"], "Total");
    assert_eq!(total["type"], "consolidated");
    assert_eq!(total["children"], json!([{"id": 1, "weight": 1.0}, {"id": 2, "weight": 2.0}]));

    // One bad item (a cycle) rejects the whole batch, naming its index.
    let r = api.post(elements, json!({"items": [{"name": "East"}, {"name": "North2", "children": [{"element": "Total"}]}, {"name": "Loop", "children": [{"element": "Loop"}]}]})).await;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let p = problem(r).await;
    assert_eq!(p["index"], 2);
    assert_eq!((&p["code"], &p["error_code"], &p["detail"]), (&json!("circular_reference"), &json!(4001), &json!("cycle detected in the element hierarchy")));
    assert_eq!(api.get(elements).await.json::<Value>().await.unwrap()["total"], 3, "nothing from the failed batch was kept");

    // PUT upserts by name: re-sending is idempotent, children get replaced.
    let batch = json!({"items": [{"name": "North"}, {"name": "West"}, {"name": "Total", "children": [{"element": "North"}, {"element": "West"}]}]});
    assert_eq!(api.put(elements, batch.clone()).await.status(), StatusCode::OK);
    let again: Value = api.put(elements, batch).await.json().await.unwrap();
    assert_eq!(again["items"][2]["children"], json!([{"id": 1, "weight": 1.0}, {"id": 3, "weight": 1.0}]));
    assert_eq!(api.get(elements).await.json::<Value>().await.unwrap()["total"], 4);

    // PATCH batch: rename, move, retype.
    let r = api.patch(elements, json!({"items": [{"element": "West", "name": "Westen", "position": 0}, {"element": 2, "type": "string"}]})).await;
    assert_eq!(r.status(), StatusCode::OK);
    let items: Value = api.get(&format!("{elements}?limit=1")).await.json().await.unwrap();
    assert_eq!(items["items"][0]["name"], "Westen");
    assert_eq!(api.get(&format!("{elements}/2")).await.json::<Value>().await.unwrap()["type"], "string");

    // Batch delete by id or name; an unknown one deletes nothing.
    let r = api.post(&format!("{elements}/delete"), json!({"items": [2, "Nope"]})).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(problem(r).await["index"], 1);
    assert_eq!(api.post(&format!("{elements}/delete"), json!({"items": [2, "Westen"]})).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(api.get(elements).await.json::<Value>().await.unwrap()["total"], 2);

    assert_eq!(api.delete(elements).await.status(), StatusCode::UNPROCESSABLE_ENTITY, "clearing needs all=true");
    assert_eq!(api.delete(&format!("{elements}?all=true")).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(api.get(elements).await.json::<Value>().await.unwrap()["total"], 0);
}

#[tokio::test]
async fn cell_batches_write_leaves_and_splash_and_read_back() {
    let api = Api::spawn().await;
    let (_, cube) = api.fixture().await;
    let cells = format!("{cube}/cells");
    assert_eq!(api.get(&cube).await.json::<Value>().await.unwrap()["modified"], Value::Null, "nothing written yet");
    let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

    let r = api
        .put(
            &cells,
            json!({"items": [
                {"path": ["North", "Jan"], "value": 10},
                {"path": ["South", "Jan"], "value": 5},
                {"path": ["North", "Feb"], "value": "note"},
                {"path": ["Total", "Q1"], "value": null},
                {"path": ["North", "Jan"], "value": 3},
                {"path": ["Total", "Feb"], "value": 30},
            ]}),
        )
        .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert!(api.get(&cube).await.json::<Value>().await.unwrap()["modified"].as_u64().unwrap() >= before, "a write sets the cube's modification time");

    let q: Value = api.post(&format!("{cells}/query"), json!({"area": [["North", "South", "Total"], ["Jan"]]})).await.json().await.unwrap();
    assert_eq!(q["items"], json!([{"path": [1, 0], "value": 3.0}, {"path": [2, 0], "value": null}, {"path": [0, 0], "value": 3.0}]));

    // An empty Total/Feb splashed 30 over North (weight 1) + South (weight 2): 10 each.
    let one: Value = api.get(&format!("{cells}/Total,Feb")).await.json().await.unwrap();
    assert_eq!(one, json!({"path": [0, 1], "value": 30.0}));
    assert_eq!(api.get(&format!("{cells}/2,1")).await.json::<Value>().await.unwrap()["value"], 10.0);

    let r: Value = api.put(&format!("{cells}/1,1"), json!({"value": 7})).await.json().await.unwrap();
    assert_eq!(r["value"], 7.0);

    // Export pages through stored base cells.
    let page: Value = api.get(&format!("{cells}?limit=2")).await.json().await.unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["next_offset"], 2);
    let rest: Value = api.get(&format!("{cells}?offset=2")).await.json().await.unwrap();
    assert_eq!(rest["items"].as_array().unwrap().len(), 1);
    assert!(rest.get("next_offset").is_none());

    // A string can't splash; the failing item's index is reported.
    let r = api.put(&cells, json!({"items": [{"path": [1, 1], "value": 1}, {"path": ["Total", "Q1"], "value": "x"}]})).await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem(r).await["index"], 1);

    let r = api.get(&format!("{cells}/1")).await;
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem(r).await["code"], "invalid_coordinates");
}

#[tokio::test]
async fn deleting_an_element_clears_its_cells_and_dimensions_in_use_are_protected() {
    let api = Api::spawn().await;
    let (region, cube) = api.fixture().await;
    api.put(&format!("{cube}/cells"), json!({"items": [{"path": [1, 1], "value": 4}, {"path": [2, 1], "value": 6}]})).await;

    assert_eq!(api.delete(&format!("{region}/elements/1")).await.status(), StatusCode::NO_CONTENT);
    let cube_dto: Value = api.get(&cube).await.json().await.unwrap();
    assert_eq!(cube_dto["cell_count"], 1);
    let page: Value = api.get(&format!("{cube}/cells")).await.json().await.unwrap();
    assert_eq!(page["items"], json!([{"path": [2, 1], "value": 6.0}]));

    let r = api.delete(&region).await;
    assert_eq!(r.status(), StatusCode::CONFLICT);
    assert_eq!(problem(r).await["code"], "dimension_in_use");

    assert_eq!(api.delete(&cube).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(api.delete(&region).await.status(), StatusCode::NO_CONTENT);
    let attr: Value = api.get("/databases/0/dimensions?type=attribute").await.json().await.unwrap();
    let names: Vec<&str> = attr["items"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap()).collect();
    assert!(!names.contains(&"#_Region_"), "attribute backing went with the dimension: {names:?}");
}

#[tokio::test]
async fn locks_journal_writes_for_rollback() {
    let api = Api::spawn().await;
    let (_, cube) = api.fixture().await;
    api.put(&format!("{cube}/cells/1,1"), json!({"value": 10})).await;

    let r = api.post(&format!("{cube}/locks"), json!({"area": [["North"], ["Jan", "Feb"]]})).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let location = r.headers()["location"].to_str().unwrap().to_string();
    let lock: Value = r.json().await.unwrap();

    let r = api.post(&format!("{cube}/locks"), json!({"area": [["Total"], ["Jan"]]})).await;
    assert_eq!(r.status(), StatusCode::CONFLICT, "overlaps the first lock");

    api.put(&format!("{cube}/cells/1,1"), json!({"value": 99})).await;
    let lock_path = location.trim_start_matches("/api/v1");
    assert_eq!(api.get(lock_path).await.json::<Value>().await.unwrap()["steps"], 1);

    assert_eq!(api.post(&format!("{lock_path}/rollback"), json!(null)).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(api.get(&format!("{cube}/cells/1,1")).await.json::<Value>().await.unwrap()["value"], 10.0);
    assert_eq!(api.get(&format!("{cube}/locks/{}", lock["id"])).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn openapi_spec_describes_every_route() {
    let api = Api::spawn().await;
    let spec: Value = api.get("/openapi.json").await.json().await.unwrap();
    let paths = spec["paths"].as_object().unwrap();
    assert_eq!(paths.len(), 24, "the docs routes aren't part of the API spec");
    let element_ops = paths["/api/v1/databases/{db}/dimensions/{dim}/elements"].as_object().unwrap();
    for method in ["get", "post", "put", "patch", "delete"] {
        assert!(element_ops.contains_key(method), "missing {method}");
    }
    // Query parameters must be documented as query, not path, parameters.
    let params = element_ops["get"]["parameters"].as_array().unwrap();
    let limit = params.iter().find(|p| p["name"] == "limit").unwrap();
    assert_eq!(limit["in"], "query");
    let page = api.get("/docs").await.text().await.unwrap();
    assert!(page.contains("swagger-ui"));
    assert!(!page.contains("https://"), "the docs page loads nothing from the internet");
}

#[tokio::test]
async fn swagger_ui_assets_are_served_from_the_binary() {
    let api = Api::spawn().await;
    let base = format!("/docs/{}", fluxolap_api::SWAGGER_UI_VERSION);
    let js = api.get(&format!("{base}/swagger-ui-bundle.js")).await;
    assert_eq!(js.status(), StatusCode::OK);
    assert!(js.headers()["content-type"].to_str().unwrap().starts_with("text/javascript"));
    assert!(js.headers()["cache-control"].to_str().unwrap().contains("immutable"));
    assert!(js.text().await.unwrap().contains("SwaggerUIBundle"));
    assert_eq!(api.get(&format!("{base}/swagger-ui.css")).await.status(), StatusCode::OK);
    assert!(api.get(&format!("{base}/LICENSE")).await.text().await.unwrap().contains("Apache License"));
    assert!(api.get(&format!("{base}/NOTICE")).await.text().await.unwrap().contains("SmartBear"));
    assert_eq!(api.get(&format!("{base}/nope.js")).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(api.get("/docs/1.0.0/swagger-ui.css").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_database_exports_and_imports_as_a_file() {
    let api = Api::spawn().await;
    let (_, cube) = api.fixture().await;
    api.put(&format!("{cube}/cells"), json!({"items": [{"path": ["North", "Jan"], "value": 4}, {"path": ["South", "Feb"], "value": "note"}]})).await;

    let r = api.get("/databases/0/export").await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()["content-type"], "application/octet-stream");
    assert_eq!(r.headers()["content-disposition"], "attachment; filename=\"Sales.bin.zst\"");
    let file = r.bytes().await.unwrap();

    // Importing while "Sales" exists is a conflict.
    let upload = |bytes: Vec<u8>| api.client.post(format!("{}/databases/import", api.base)).header("content-type", "application/octet-stream").body(bytes).send();
    let r = upload(file.to_vec()).await.unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
    assert_eq!(problem(r).await["code"], "database_name_in_use");

    assert_eq!(api.delete("/databases/0").await.status(), StatusCode::NO_CONTENT);
    let r = upload(file.to_vec()).await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let location = r.headers()["location"].to_str().unwrap().trim_start_matches("/api/v1").to_string();
    let db: Value = r.json().await.unwrap();
    assert_eq!(db["name"], "Sales");

    let cubes: Value = api.get(&format!("{location}/cubes?name=Revenue")).await.json().await.unwrap();
    let cube = format!("{location}/cubes/{}", cubes["items"][0]["id"]);
    let q: Value = api.post(&format!("{cube}/cells/query"), json!({"paths": [["North", "Jan"], ["South", "Feb"], ["Total", "Jan"]]})).await.json().await.unwrap();
    let values: Vec<&Value> = q["items"].as_array().unwrap().iter().map(|c| &c["value"]).collect();
    assert_eq!(values, [&json!(4.0), &json!("note"), &json!(4.0)]);

    let r = upload(b"definitely not a database".to_vec()).await.unwrap();
    assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(problem(r).await["code"], "invalid_database_file");
}

async fn spawn_with(options: fluxolap_api::Options) -> String {
    let app = fluxolap_api::router_with(fluxolap_engine::AppState::new(), options);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn preflight(server: &str, origin: &str) -> reqwest::Response {
    Client::new()
        .request(reqwest::Method::OPTIONS, format!("{server}/api/v1/databases"))
        .header("origin", origin)
        .header("access-control-request-method", "POST")
        .header("access-control-request-headers", "content-type")
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn cors_is_off_by_default_and_allows_configured_origins() {
    let plain = spawn_with(Default::default()).await;
    assert!(preflight(&plain, "http://localhost:8080").await.headers().get("access-control-allow-origin").is_none());

    let server = spawn_with(fluxolap_api::Options { cors: fluxolap_api::Cors::parse("http://localhost:8080, https://olap.example.com/"), ..Default::default() }).await;
    let r = preflight(&server, "https://olap.example.com").await;
    assert_eq!(r.headers()["access-control-allow-origin"], "https://olap.example.com");
    assert!(preflight(&server, "https://evil.example.com").await.headers().get("access-control-allow-origin").is_none());
    let r = Client::new().get(format!("{server}/api/v1/databases")).header("origin", "http://localhost:8080").send().await.unwrap();
    assert_eq!(r.headers()["access-control-allow-origin"], "http://localhost:8080");
    assert!(r.headers()["access-control-expose-headers"].to_str().unwrap().to_lowercase().contains("location"));

    let any = spawn_with(fluxolap_api::Options { cors: fluxolap_api::Cors::parse("*"), ..Default::default() }).await;
    assert_eq!(preflight(&any, "https://anywhere.example").await.headers()["access-control-allow-origin"], "*");
}

#[tokio::test]
async fn a_web_app_directory_is_served_at_ui() {
    let dir = std::env::temp_dir().join(format!("fluxolap-ui-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("assets")).unwrap();
    std::fs::write(dir.join("index.html"), "<html>fluxoscope</html>").unwrap();
    std::fs::write(dir.join("assets/app.css"), "body{}").unwrap();
    let server = spawn_with(fluxolap_api::Options { ui_dir: Some(dir.clone()), ..Default::default() }).await;
    let client = Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();

    assert_eq!(client.get(format!("{server}/")).send().await.unwrap().headers()["location"], "/ui/");
    assert_eq!(client.get(format!("{server}/ui")).send().await.unwrap().headers()["location"], "/ui/");
    assert_eq!(client.get(format!("{server}/ui/")).send().await.unwrap().text().await.unwrap(), "<html>fluxoscope</html>");
    assert_eq!(client.get(format!("{server}/ui/assets/app.css")).send().await.unwrap().text().await.unwrap(), "body{}");
    assert_eq!(client.get(format!("{server}/api/v1")).send().await.unwrap().status(), StatusCode::OK, "the API still works");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn element_lists_can_be_limited_to_top_level_elements() {
    let api = Api::spawn().await;
    let (region, _) = api.fixture().await;
    let roots: Value = api.get(&format!("{region}/elements?roots=true")).await.json().await.unwrap();
    let names: Vec<&str> = roots["items"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Total"]);
}

#[tokio::test]
async fn example_files_are_listed_and_imported_by_name() {
    // An example file: the fixture database, exported.
    let source = Api::spawn().await;
    source.fixture().await;
    let bytes = source.get("/databases/0/export").await.bytes().await.unwrap();
    let dir = std::env::temp_dir().join(format!("fluxolap-examples-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("db-7-Sales.bin.zst"), &bytes).unwrap();
    std::fs::write(dir.join("registry.bin.zst"), &bytes).unwrap();
    std::fs::write(dir.join("junk.bin.zst"), b"not a database").unwrap();
    std::fs::write(dir.join("notes.txt"), b"hello").unwrap();

    let server = spawn_with(fluxolap_api::Options { examples_dirs: vec![dir.join("missing"), dir.clone()], ..Default::default() }).await;
    let client = Client::new();
    let list = || async { client.get(format!("{server}/api/v1/examples")).send().await.unwrap().json::<Value>().await.unwrap() };
    let import = |file: &str| client.post(format!("{server}/api/v1/examples/{file}/import")).send();

    let examples = list().await;
    assert_eq!(examples["total"], 1, "only real database files, not the registry: {examples}");
    assert_eq!(examples["items"][0], json!({"file": "db-7-Sales.bin.zst", "name": "Sales", "size": bytes.len(), "loaded": false}));

    let r = import("db-7-Sales.bin.zst").await.unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    assert_eq!(r.json::<Value>().await.unwrap()["name"], "Sales");
    assert_eq!(list().await["items"][0]["loaded"], true);
    assert_eq!(import("db-7-Sales.bin.zst").await.unwrap().status(), StatusCode::CONFLICT);

    for file in ["notes.txt", "registry.bin.zst", "..%2Fdb-7-Sales.bin.zst", "nope.bin.zst"] {
        let r = import(file).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{file}");
        assert_eq!(r.json::<Value>().await.unwrap()["code"], "example_not_found");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn stats_report_memory_contents_and_requests() {
    let api = Api::spawn().await;
    let (_, cube) = api.fixture().await;
    api.put(&format!("{cube}/cells"), json!({"items": [{"path": ["North", "Jan"], "value": 4}, {"path": ["South", "Feb"], "value": "note"}]})).await;
    api.post(&format!("{cube}/locks"), json!({"area": [["North"], ["Jan"]]})).await;
    assert_eq!(api.get("/no/such/route").await.status(), StatusCode::NOT_FOUND);

    let stats: Value = api.get("/stats").await.json().await.unwrap();
    assert_eq!(stats["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(stats["pid"], std::process::id());
    assert_eq!(stats["totals"]["databases"], 1);
    assert_eq!(stats["totals"]["cells"], 2);
    let db = &stats["databases"][0];
    assert_eq!((db["name"].as_str(), db["loaded"].as_bool(), db["cells"].as_u64()), (Some("Sales"), Some(true), Some(2)));
    let revenue = db["cubes"].as_array().unwrap().iter().find(|c| c["name"] == "Revenue").unwrap();
    assert_eq!((revenue["type"].as_str(), revenue["dimensions"].as_u64(), revenue["cells"].as_u64()), (Some("normal"), Some(2), Some(2)));
    assert_eq!((revenue["hot_cells"].as_u64(), revenue["locks"].as_u64()), (Some(2), Some(1)));
    assert!(revenue["hot_bytes"].as_u64().unwrap() > 0);
    assert_eq!(stats["memory"]["data_bytes"], db["bytes"]);
    if cfg!(target_os = "linux") {
        assert!(stats["memory"]["rss_bytes"].as_u64().unwrap() > 1024 * 1024, "{}", stats["memory"]);
        assert!(stats["memory"]["system_total_bytes"].as_u64().is_some());
    }

    let requests = &stats["requests"];
    assert_eq!(requests["total"], 9, "6 in the fixture, the cell write, the lock and the unknown route: {requests}");
    assert_eq!(requests["client_errors"], 1, "the unknown route");
    assert_eq!(requests["in_flight"], 0);
    let again: Value = api.get("/stats").await.json().await.unwrap();
    assert_eq!(again["requests"]["total"], requests["total"], "reading the stats isn't counted");
}

#[tokio::test]
async fn a_dimension_reports_when_it_last_changed() {
    let api = Api::spawn().await;
    let before = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let (region, _) = api.fixture().await;
    let modified = api.get(&region).await.json::<Value>().await.unwrap()["modified"].as_u64().expect("a dimension's modification time");
    assert!(modified >= before, "creating the dimension and its elements sets it");
}

#[tokio::test]
async fn skip_empty_leaves_out_cells_without_a_value() {
    let api = Api::spawn().await;
    let (_, cube) = api.fixture().await;
    api.put(&format!("{cube}/cells"), json!({"items": [{"path": ["North", "Jan"], "value": 4}]})).await;
    let area = json!([["Total", "North", "South"], ["Jan", "Feb", "Q1"]]);
    let all: Value = api.post(&format!("{cube}/cells/query"), json!({"area": area})).await.json().await.unwrap();
    assert_eq!(all["total"], 9);
    let sparse: Value = api.post(&format!("{cube}/cells/query"), json!({"area": area, "skip_empty": true})).await.json().await.unwrap();
    // North/Jan, and the totals it rolls into: Total/Jan, North/Q1, Total/Q1.
    let paths: Vec<&Value> = sparse["items"].as_array().unwrap().iter().map(|c| &c["path"]).collect();
    assert_eq!(paths, [&json!([0, 0]), &json!([0, 2]), &json!([1, 0]), &json!([1, 2])]);
    assert_eq!(sparse["total"], 4);
}

#[tokio::test]
async fn the_mesh_endpoints_are_a_404_problem_without_a_mesh() {
    let api = Api::spawn().await;
    for r in [api.get("/mesh").await, api.send(reqwest::Method::POST, "/mesh/friends/b/databases/Sales/import", None).await] {
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        assert_eq!(r.json::<Value>().await.unwrap()["code"], "mesh_disabled");
    }
    let spec: Value = api.get("/openapi.json").await.json().await.unwrap();
    assert!(spec["paths"]["/api/v1/mesh"]["get"].is_object() && spec["paths"]["/api/v1/mesh/friends/{friend}/databases/{database}/import"]["post"].is_object());
}
