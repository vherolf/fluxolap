//! The page: header (server), sidebar (databases and cubes), and the
//! cube explorer.

use crate::api::Api;
use crate::explorer::ExplorerView;
use crate::load::LoadView;
use crate::model::format::group;
use crate::state::{Ctx, ViewState};
use fluxolap_api_types::ObjectType;
use leptos::prelude::*;

const SERVER_KEY: &str = "fluxoscope.server";
/// How often the page re-reads the server.
const POLL_SECS: u64 = 5;

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

#[component]
pub fn App() -> impl IntoView {
    let saved = storage().and_then(|s| s.get_item(SERVER_KEY).ok().flatten()).unwrap_or_default();
    let ctx = Ctx::new(Api::new(&saved));
    provide_context(ctx);
    ctx.connect();
    // Keep up with the server: changes made elsewhere (fluxoterm, the API)
    // show up within a few seconds.
    set_interval(move || ctx.poll(), std::time::Duration::from_secs(POLL_SECS));
    // A link to a view opens it.
    if let Some(view) = web_sys::window().and_then(|w| w.location().hash().ok()).and_then(|h| ViewState::from_hash(&h)) {
        ctx.open_database(view.db, Some((view.cube, Some(view))));
    }
    let exploring = Memo::new(move |_| ctx.explorer.with(Option::is_some));
    view! {
        <div class="app">
            <Header/>
            <div class="body">
                <Sidebar/>
                <main class="main">
                    {move || {
                        if ctx.show_load.get() {
                            view! { <LoadView/> }.into_any()
                        } else if exploring.get() {
                            view! { <ExplorerView/> }.into_any()
                        } else {
                            view! { <Welcome/> }.into_any()
                        }
                    }}
                </main>
            </div>
        </div>
    }
}

#[component]
fn Header() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    let draft = RwSignal::new(ctx.api.get_untracked().base);
    let connect = move || {
        let api = Api::new(&draft.get_untracked());
        if let Some(s) = storage() {
            let _ = s.set_item(SERVER_KEY, &api.base);
        }
        draft.set(api.base.clone());
        ctx.api.set(api);
        ctx.explorer.set(None);
        ctx.open_db.set(None);
        ctx.contents.set(Default::default());
        ctx.databases.set(Vec::new());
        ctx.server.set(None);
        ctx.show_load.set(false);
        ctx.examples.set(None);
        ctx.connect();
    };
    let status = move || match ctx.server.get() {
        None => "connecting …".to_string(),
        Some(Ok(s)) => format!("{} {}", s.name, s.version),
        Some(Err(e)) => format!("offline: {e}"),
    };
    view! {
        <header class="header">
            <span class="logo">"flux" <b>"oscope"</b></span>
            <form class="server" on:submit=move |ev| { ev.prevent_default(); connect(); }>
                <input
                    placeholder="fluxolap server (empty: this site)"
                    prop:value=move || draft.get()
                    on:input=move |ev| draft.set(event_target_value(&ev))
                />
                <button type="submit">"Connect"</button>
            </form>
            <span class="status" class:offline=move || matches!(ctx.server.get(), Some(Err(_)))>{status}</span>
        </header>
    }
}

#[component]
fn Sidebar() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    view! {
        <nav class="sidebar">
            <h2>"Databases"</h2>
            {move || {
                ctx.databases
                    .get()
                    .into_iter()
                    .map(|db| {
                        let id = db.id;
                        let open = move || ctx.open_db.get() == Some(id);
                        view! {
                            <div class="db">
                                <button
                                    class="db-name"
                                    class:open=open
                                    on:click=move |_| {
                                        if ctx.open_db.get_untracked() == Some(id) { ctx.open_db.set(None) } else { ctx.open_database(id, None) }
                                    }
                                >
                                    {db.name.clone()}
                                </button>
                                {move || open().then(|| view! { <CubeList db=id/> })}
                            </div>
                        }
                    })
                    .collect_view()
            }}
            <button class="load-link" class:active=move || ctx.show_load.get() on:click=move |_| ctx.open_load()>"+ Load database"</button>
        </nav>
    }
}

#[component]
fn CubeList(db: u32) -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    move || {
        let cubes = ctx.contents.with(|c| c.get(&db).map(|(_, cubes)| cubes.clone()));
        let Some(cubes) = cubes else { return view! { <p class="muted small">"loading …"</p> }.into_any() };
        view! {
            <ul class="cubes">
                {cubes
                    .into_iter()
                    .filter(|c| c.cube_type == ObjectType::Normal)
                    .map(|cube| {
                        let id = cube.id;
                        let active = move || ctx.explorer.with(|e| e.as_ref().is_some_and(|e| e.db == db && e.cube.id == id));
                        let name = cube.name.clone();
                        let cells = group(cube.cell_count as f64);
                        view! {
                            <li>
                                <button class="cube" class:active=active on:click=move |_| ctx.open_cube(db, cube.clone(), None)>
                                    <span>{name}</span>
                                    <span class="count">{cells}</span>
                                </button>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        }
        .into_any()
    }
}

#[component]
fn Welcome() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    view! {
        <div class="welcome">
            <h1>"Explore your cubes"</h1>
            <p>"Pick a database on the left, then a cube. You get a pivot table you can drill into, and line, bar and stacked charts of the same view."</p>
            <p class="muted">
                "Rows and columns can each hold several dimensions; every other dimension is a filter. "
                "Every view has its own address -- bookmark it or send it on."
            </p>
            <p><button class="primary" on:click=move |_| ctx.open_load()>"Load a database"</button>" -- an example, or a file from your computer."</p>
        </div>
    }
}
