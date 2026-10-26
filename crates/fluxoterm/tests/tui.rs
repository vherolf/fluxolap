//! fluxoterm end to end: a real server (the API router on an ephemeral
//! port), the app driven by key presses, requests run by `exec`, and the
//! screen rendered to an in-memory terminal.

use fluxoterm::app::{format_number, App, Cmd, Popup};
use fluxoterm::client::Client;
use fluxoterm::{exec, ui};
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use serde_json::json;

async fn spawn_server(state: fluxolap_engine::AppState) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, fluxolap_api::router(state)).await.unwrap() });
    format!("http://{addr}")
}

struct Tui {
    app: App,
    client: Client,
    terminal: Terminal<TestBackend>,
}

impl Tui {
    async fn start(server: &str) -> Self {
        Self::start_with(server, Vec::new()).await
    }

    /// Starts with `search_dirs` as the places to look for database files.
    async fn start_with(server: &str, search_dirs: Vec<String>) -> Self {
        let client = Client::new(server);
        let mut tui = Self { app: App::new(client.server_url().to_string()), client, terminal: Terminal::new(TestBackend::new(140, 36)).unwrap() };
        tui.app.search_dirs = search_dirs;
        let cmds = tui.app.start();
        tui.run(cmds).await;
        tui
    }

    /// Runs requests until nothing new is asked for, redrawing in between
    /// (the grid decides what to load when it's drawn).
    async fn run(&mut self, mut cmds: Vec<Cmd>) {
        for _ in 0..20 {
            for cmd in std::mem::take(&mut cmds) {
                let msg = exec::execute(&self.client, cmd).await;
                cmds.extend(self.app.on_msg(msg));
            }
            self.terminal.draw(|f| ui::draw(f, &mut self.app)).unwrap();
            cmds.extend(self.app.after_draw());
            if cmds.is_empty() {
                return;
            }
        }
        panic!("requests never settled");
    }

    async fn key(&mut self, code: KeyCode) {
        let cmds = self.app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        self.run(cmds).await;
    }

    async fn keys(&mut self, codes: &[KeyCode]) {
        for &c in codes {
            self.key(c).await;
        }
    }

    async fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.key(KeyCode::Char(c)).await;
        }
    }

    /// Replaces the open input popup's (or the picker's) text.
    async fn set_input(&mut self, text: &str) {
        let len = match &self.app.popup {
            Some(Popup::Input { value, .. }) => value.chars().count(),
            Some(Popup::Picker { input, .. }) => input.chars().count(),
            other => panic!("no input open: {other:?}"),
        };
        for _ in 0..len {
            self.key(KeyCode::Backspace).await;
        }
        self.type_text(text).await;
    }

    fn screen(&self) -> String {
        let buffer = self.terminal.backend().buffer();
        let width = buffer.area.width as usize;
        buffer.content().chunks(width).map(|row| row.iter().map(|c| c.symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }
}

#[tokio::test]
async fn an_empty_server_is_offered_the_examples_and_population_opens_on_the_world_hierarchy() {
    let out = std::env::temp_dir().join(format!("fluxoterm-examples-{}", std::process::id()));
    fluxolap_examples::build_all(&fluxolap_examples::default_sources_dir(), &out).unwrap();
    let server = spawn_server(fluxolap_engine::AppState::new()).await;

    // An empty server: fluxoterm offers the database files it finds.
    let mut tui = Tui::start_with(&server, vec![out.to_string_lossy().into_owned()]).await;
    let screen = tui.screen();
    assert!(screen.contains("The server has no databases yet"), "{screen}");
    for name in ["Load all 8", "Climate", "Exoplanets", "GeneticVariants", "Metabolomics", "NobelPrizes", "OnlineRetail", "ParticlePhysics", "WorldPopulation"] {
        assert!(screen.contains(name), "{name} offered:\n{screen}");
    }
    tui.key(KeyCode::Enter).await; // "Load all"
    let names: Vec<String> = tui.app.browser.databases.iter().map(|d| d.name.clone()).collect();
    assert_eq!(names, ["Climate", "Exoplanets", "GeneticVariants", "Metabolomics", "NobelPrizes", "OnlineRetail", "ParticlePhysics", "WorldPopulation"]);

    // Loaded ones are marked in the picker (EDIT mode, o).
    tui.keys(&[KeyCode::Char('i'), KeyCode::Char('o')]).await;
    assert!(tui.screen().contains("✓ on the server") && !tui.screen().contains("Load all"), "{}", tui.screen());
    tui.keys(&[KeyCode::Esc, KeyCode::Esc]).await; // close the picker, back to VIEW
    assert_eq!(tui.app.mode, fluxoterm::app::Mode::View);

    // WorldPopulation (the 8th) -> its Population cube.
    tui.keys(&[KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Down, KeyCode::Enter]).await;
    assert!(tui.screen().contains("Population"), "{}", tui.screen());
    tui.key(KeyCode::Enter).await;
    let screen = tui.screen();
    // Rows start on World, opened to its regions; columns are the years;
    // Sex and Age are filters on their totals.
    for text in ["▾ World", "▸ Africa", "▸ Asia", "1950", "1955", "Sex:", "Both sexes", "Age:", "All ages"] {
        assert!(screen.contains(text), "{text:?} on screen:\n{screen}");
    }
    let world_1950 = Client::new(&server).area(7, 5, vec![vec![0], vec![0], vec![0], vec![0]]).await.unwrap()[0].value.clone();
    let Some(fluxolap_api_types::Value::Number(n)) = world_1950 else { panic!("World 1950 is {world_1950:?}") };
    assert!(screen.contains(&format_number(n)), "World 1950 = {} shown grouped:\n{screen}", format_number(n));
    assert!(format_number(n).contains(','), "a population reads with thousands separators");

    // A value changed elsewhere (fluxoscope, Calc, the API) shows up after the next refresh.
    Client::new(&server).write_cell(7, 5, &[0, 0, 0, 0], Some(fluxolap_api_types::Value::Number(9_999_999_999.0))).await.unwrap();
    assert!(!tui.screen().contains("9,999,999,999"));
    tui.app.last_refresh = std::time::Instant::now() - fluxoterm::app::REFRESH_EVERY;
    let cmds = tui.app.tick();
    tui.run(cmds).await;
    assert!(tui.screen().contains("9,999,999,999"), "the grid read its cells again:\n{}", tui.screen());
    assert!(tui.screen().contains("↻ every 5 s"), "WorldPopulation is small: on by default");
    // `a` switches it off: the next change waits for `r`.
    tui.key(KeyCode::Char('a')).await;
    assert!(tui.screen().contains("↻ off"), "{}", tui.screen());
    Client::new(&server).write_cell(7, 5, &[0, 0, 0, 0], Some(fluxolap_api_types::Value::Number(8_888_888_888.0))).await.unwrap();
    tui.app.last_refresh = std::time::Instant::now() - fluxoterm::app::REFRESH_EVERY;
    let cmds = tui.app.tick();
    tui.run(cmds).await;
    assert!(!tui.screen().contains("8,888,888,888"), "not refreshed on its own");
    tui.key(KeyCode::Char('r')).await;
    assert!(tui.screen().contains("8,888,888,888"), "r reads once:\n{}", tui.screen());
    tui.key(KeyCode::Char('a')).await;

    // The filter: next element of the focused dimension (Sex) is Male.
    tui.key(KeyCode::Char(']')).await;
    assert!(tui.screen().contains("◂ Male ▸"), "{}", tui.screen());

    // The server's statistics (S): memory, requests, and the databases by
    // memory, the largest first.
    assert!(tui.screen().contains("mem "), "the header shows the server's memory:\n{}", tui.screen());
    tui.key(KeyCode::Char('S')).await;
    let screen = tui.screen();
    for text in ["Server", "Memory", "Requests", "Databases & cubes by memory", "cell data", "req/s", "Population", "Exoplanets"] {
        assert!(screen.contains(text), "{text:?} on the stats screen:\n{screen}");
    }
    let first_db = names.iter().min_by_key(|name| screen.find(name.as_str()).unwrap()).unwrap();
    let stats = Client::new(&server).stats().await.unwrap().unwrap();
    let largest = stats.databases.iter().max_by_key(|d| d.bytes).unwrap();
    assert_eq!(*first_db, largest.name, "largest first:\n{screen}");
    tui.key(KeyCode::Char('q')).await;
    assert!(tui.screen().contains("◂ Male ▸"), "back to the grid:\n{}", tui.screen());
    let _ = std::fs::remove_dir_all(&out);
}

#[tokio::test]
async fn edit_export_delete_and_import_through_the_ui() {
    let server = spawn_server(fluxolap_engine::AppState::new()).await;
    let http = reqwest::Client::new();
    let api = format!("{server}/api/v1");
    let post = |path: &str, body: serde_json::Value| http.post(format!("{api}{path}")).json(&body).send();
    post("/databases", json!({"name": "Shop"})).await.unwrap();
    post("/databases/0/dimensions", json!({"name": "Region"})).await.unwrap();
    post("/databases/0/dimensions", json!({"name": "Month"})).await.unwrap();
    post("/databases/0/dimensions/2/elements", json!({"items": [{"name": "Total", "children": [{"element": "North"}, {"element": "South"}]}, {"name": "North"}, {"name": "South"}]})).await.unwrap();
    post("/databases/0/dimensions/4/elements", json!({"items": [{"name": "Jan"}, {"name": "Feb"}]})).await.unwrap();
    post("/databases/0/cubes", json!({"name": "Sales", "dimensions": ["Region", "Month"]})).await.unwrap();
    http.put(format!("{api}/databases/0/cubes/3/cells")).json(&json!({"items": [{"path": ["North", "Jan"], "value": 1234567}]})).send().await.unwrap();

    let mut tui = Tui::start(&server).await;
    tui.keys(&[KeyCode::Enter, KeyCode::Enter]).await; // into Shop, open Sales
    let screen = tui.screen();
    assert!(screen.contains("1,234,567") && screen.contains("▾ Total"), "{screen}");

    // VIEW mode (the default) can't change anything.
    tui.keys(&[KeyCode::Down, KeyCode::Down, KeyCode::Right]).await;
    assert!(tui.screen().contains(" VIEW "), "{}", tui.screen());
    tui.key(KeyCode::Delete).await;
    assert!(tui.screen().contains("read-only in VIEW mode"), "{}", tui.screen());
    tui.key(KeyCode::Enter).await;
    assert!(tui.app.popup.is_none(), "Enter in VIEW mode doesn't open the editor");

    // EDIT mode: edit South/Feb (row 2, column 1); the total follows.
    tui.keys(&[KeyCode::Char('i'), KeyCode::Enter]).await;
    assert!(tui.screen().contains(" EDIT "), "{}", tui.screen());
    tui.type_text("1000").await;
    tui.key(KeyCode::Enter).await;
    let screen = tui.screen();
    assert!(screen.contains("cell saved"), "{screen}");
    let total_feb = Client::new(&server).area(0, 3, vec![vec![0], vec![1]]).await.unwrap()[0].value.clone();
    assert_eq!(total_feb, Some(fluxolap_api_types::Value::Number(1000.0)));

    // Back to VIEW and to the databases; exporting is allowed in VIEW (`:w file`).
    tui.keys(&[KeyCode::Esc, KeyCode::Esc, KeyCode::Esc]).await;
    assert_eq!(tui.app.mode, fluxoterm::app::Mode::View);
    let file = std::env::temp_dir().join(format!("fluxoterm-export-{}.bin.zst", std::process::id()));
    tui.key(KeyCode::Char(':')).await;
    tui.type_text(&format!("w {}", file.display())).await;
    tui.key(KeyCode::Enter).await;
    assert!(tui.screen().contains("exported"), "{}", tui.screen());
    assert!(file.exists());

    // Loading is a change: refused in VIEW.
    tui.key(KeyCode::Char(':')).await;
    tui.type_text("e").await;
    tui.key(KeyCode::Enter).await;
    assert!(tui.app.popup.is_none() && tui.screen().contains("read-only"), "{}", tui.screen());

    // EDIT: dd deletes (after confirming), o loads.
    tui.keys(&[KeyCode::Char('i'), KeyCode::Char('d'), KeyCode::Char('d'), KeyCode::Char('y')]).await;
    assert!(tui.screen().contains("database deleted"), "{}", tui.screen());
    assert!(tui.app.browser.databases.is_empty());

    tui.key(KeyCode::Char('o')).await;
    tui.set_input(file.to_str().unwrap()).await;
    tui.key(KeyCode::Enter).await;
    assert!(tui.screen().contains("imported database 'Shop'"), "{}", tui.screen());
    let names: Vec<&str> = tui.app.browser.databases.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["Shop"]);
    let _ = std::fs::remove_file(&file);

    tui.key(KeyCode::Char(':')).await;
    tui.type_text("q").await;
    tui.key(KeyCode::Enter).await;
    assert!(tui.app.quit);
}
