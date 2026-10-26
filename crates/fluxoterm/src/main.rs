//! `fluxoterm [+COMMAND]... [SERVER]` -- a terminal viewer/editor for a fluxolap server.

use fluxoterm::app::{App, Msg};
use fluxoterm::client::Client;
use fluxoterm::{exec, ui};
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use ratatui::DefaultTerminal;
use std::sync::mpsc;
use std::time::Duration;

const USAGE: &str = "fluxoterm -- terminal viewer/editor for a fluxolap server

usage: fluxoterm [+COMMAND]... [SERVER]

SERVER is the server's address, default http://localhost:7777
(or the FLUXOTERM_URL environment variable).

+COMMAND runs a : command right after connecting, like vim's +cmd:
  fluxoterm +stats                 start in the server statistics view
  fluxoterm +stats http://host:7777

Press ? inside for keys.";

/// What the command line asks for.
#[derive(Debug, PartialEq)]
enum Args {
    Run { server: Option<String>, commands: Vec<String> },
    Help,
    Version,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let (mut server, mut commands) = (None, Vec::new());
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Args::Help),
            "-V" | "--version" => return Ok(Args::Version),
            _ if arg.starts_with('+') => commands.push(arg[1..].to_string()),
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}")),
            _ if server.is_some() => return Err(format!("more than one server given: {arg}")),
            _ => server = Some(arg),
        }
    }
    Ok(Args::Run { server, commands })
}

fn main() -> std::io::Result<()> {
    let (server, commands) = match parse_args(std::env::args().skip(1)) {
        Ok(Args::Run { server, commands }) => (server, commands),
        Ok(Args::Help) => {
            println!("{USAGE}");
            return Ok(());
        }
        Ok(Args::Version) => {
            println!("fluxoterm {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Err(e) => {
            eprintln!("fluxoterm: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let server = server.or_else(|| std::env::var("FLUXOTERM_URL").ok()).unwrap_or_else(|| "http://localhost:7777".to_string());
    let client = Client::new(&server);
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let mut app = App::new(client.server_url().to_string());
    app.search_dirs = exec::default_search_dirs();
    app.startup_commands = commands;

    // `ratatui::init` also restores the terminal if we panic.
    let mut terminal = ratatui::init();
    let result = run(&mut terminal, &mut app, &client, &runtime);
    ratatui::restore();
    result
}

/// The UI loop: draw, read a key (or time out), take finished requests,
/// start new ones. Requests run on the tokio runtime and report back over
/// a channel, so the screen never waits for the network.
fn run(terminal: &mut DefaultTerminal, app: &mut App, client: &Client, runtime: &tokio::runtime::Runtime) -> std::io::Result<()> {
    let (tx, rx) = mpsc::channel::<Msg>();
    let mut cmds = app.start();
    loop {
        for cmd in cmds.drain(..) {
            let (client, tx) = (client.clone(), tx.clone());
            runtime.spawn(async move {
                let _ = tx.send(exec::execute(&client, cmd).await);
            });
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
        cmds.extend(app.after_draw());
        if app.quit {
            return Ok(());
        }
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    cmds.extend(app.on_key(key));
                }
            }
        }
        while let Ok(msg) = rx.try_recv() {
            cmds.extend(app.on_msg(msg));
        }
        cmds.extend(app.tick());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|a| a.to_string()))
    }

    #[test]
    fn plus_commands_and_the_server_in_any_order() {
        assert_eq!(parse(&[]), Ok(Args::Run { server: None, commands: vec![] }));
        assert_eq!(parse(&["+stats"]), Ok(Args::Run { server: None, commands: vec!["stats".into()] }));
        assert_eq!(parse(&["http://h:7778", "+stats"]), Ok(Args::Run { server: Some("http://h:7778".into()), commands: vec!["stats".into()] }));
        assert_eq!(parse(&["--help", "+stats"]), Ok(Args::Help));
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["a", "b"]).is_err());
    }
}
