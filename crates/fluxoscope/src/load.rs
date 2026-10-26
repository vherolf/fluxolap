//! Loading databases into the server, like fluxoterm's load list: the
//! server's example databases (it reads them from its own disk), or
//! database files (`.bin.zst`) uploaded from this computer.

use crate::api::Result;
use crate::model::format::file_size;
use crate::state::Ctx;
use fluxolap_api_types::{DatabaseDto, ExampleDto};
use leptos::prelude::*;
use leptos::task::spawn_local;
use std::collections::HashSet;

#[component]
pub fn LoadView() -> impl IntoView {
    let ctx = expect_context::<Ctx>();
    // Files being imported, and what happened so far (newest first).
    let busy = RwSignal::new(HashSet::<String>::new());
    let log = RwSignal::new(Vec::<std::result::Result<String, String>>::new());
    let done = move |label: String, result: Result<DatabaseDto>| {
        busy.update(|b| {
            b.remove(&label);
        });
        match result {
            Ok(db) => {
                log.update(|l| l.insert(0, Ok(format!("Loaded '{}' -- pick a cube on the left.", db.name))));
                ctx.imported(&db);
            }
            Err(e) => log.update(|l| l.insert(0, Err(format!("{label}: {e}")))),
        }
    };
    let load_examples = move |files: Vec<String>| {
        busy.update(|b| b.extend(files.iter().cloned()));
        let api = ctx.api.get_untracked();
        // One after the other: each import takes the server's database lock anyway.
        spawn_local(async move {
            for file in files {
                let result = api.import_example(&file).await;
                done(file, result);
            }
        });
    };
    let upload = move |ev: leptos::ev::Event| {
        let input: web_sys::HtmlInputElement = event_target(&ev);
        let Some(list) = input.files() else { return };
        let files: Vec<web_sys::File> = (0..list.length()).filter_map(|i| list.get(i)).collect();
        input.set_value("");
        busy.update(|b| b.extend(files.iter().map(|f| f.name())));
        let api = ctx.api.get_untracked();
        spawn_local(async move {
            for file in files {
                let name = file.name();
                let result = api.import_file(file).await;
                done(name, result);
            }
        });
    };
    let open = move |name: String| {
        if let Some(id) = ctx.databases.with_untracked(|dbs| dbs.iter().find(|d| d.name == name).map(|d| d.id)) {
            ctx.show_load.set(false);
            ctx.open_database(id, None);
        }
    };

    let examples = move || match ctx.examples.get() {
        None => view! { <p class="muted">"asking the server …"</p> }.into_any(),
        Some(Err(e)) => view! { <p class="error">{e}</p> }.into_any(),
        Some(Ok(list)) if list.is_empty() => view! {
            <p class="muted">
                "This server offers no examples. Start it with "<code>"FLUXOLAP_EXAMPLES=<dir>"</code>
                ", or keep the release's "<code>"examples/"</code>" folder next to the fluxolap binary."
            </p>
        }
        .into_any(),
        Some(Ok(list)) => {
            let pending: Vec<String> = list.iter().filter(|e| !e.loaded).map(|e| e.file.clone()).collect();
            let rows = list.into_iter().map(|example| example_row(example, busy, load_examples, open)).collect_view();
            view! {
                <table class="examples">
                    <tbody>{rows}</tbody>
                </table>
                {(pending.len() > 1).then(|| {
                    let count = pending.len();
                    let all_busy = move || busy.with(|b| !b.is_empty());
                    view! {
                        <p><button class="primary" disabled=all_busy on:click=move |_| load_examples(pending.clone())>{format!("Load all {count}")}</button></p>
                    }
                })}
            }
            .into_any()
        }
    };

    view! {
        <div class="load">
            <h1>"Load a database"</h1>
            <p class="muted">
                "A loaded database is on the server for everyone using it, until it is deleted or the server stops "
                "(with " <code>"FLUXOLAP_DATA_DIR"</code> " set, it is saved and comes back)."
            </p>
            <h2>"Examples on the server"</h2>
            {examples}
            <h2>"From this computer"</h2>
            <p class="muted">"A database file (" <code>".bin.zst"</code> "), as fluxoterm or " <code>"GET …/export"</code> " saves it."</p>
            <label class="upload">
                <input type="file" accept=".zst" multiple on:change=upload/>
                "Choose files …"
            </label>
            <ul class="log">
                {move || {
                    log.get()
                        .into_iter()
                        .map(|entry| match entry {
                            Ok(text) => view! { <li class="ok">{text}</li> },
                            Err(text) => view! { <li class="error">{text}</li> },
                        })
                        .collect_view()
                }}
            </ul>
        </div>
    }
}

fn example_row(
    example: ExampleDto,
    busy: RwSignal<HashSet<String>>,
    load: impl Fn(Vec<String>) + Copy + Send + Sync + 'static,
    open: impl Fn(String) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let ExampleDto { file, name, size, loaded } = example;
    let working = {
        let file = file.clone();
        move || busy.with(|b| b.contains(&file))
    };
    let action = if loaded {
        let name = name.clone();
        view! { <span class="muted">"loaded "</span><button class="plain" on:click=move |_| open(name.clone())>"open"</button> }.into_any()
    } else {
        let file = file.clone();
        view! {
            {move || if working() {
                view! { <span class="spinner">"loading …"</span> }.into_any()
            } else {
                let file = file.clone();
                view! { <button on:click=move |_| load(vec![file.clone()])>"Load"</button> }.into_any()
            }}
        }
        .into_any()
    };
    view! {
        <tr>
            <td class="example-name">{name}</td>
            <td class="muted small">{file}</td>
            <td class="num muted small">{file_size(size)}</td>
            <td class="action">{action}</td>
        </tr>
    }
}
