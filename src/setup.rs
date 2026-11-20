//! First-run setup wizard (`riptune setup`, and automatically when no config
//! exists). Asks for the server details, checks them with a real `ping`
//! before saving anything, and writes a config readable only by the user,
//! since it contains a password.

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use riptune_core::{SubsonicClient, SubsonicError};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;

const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const RESET: &str = "\x1b[0m";

#[derive(Default)]
struct Answers {
    url: String,
    username: String,
    password: String,
}

pub async fn run(config_path: &Path) -> Result<()> {
    if !io::stdin().is_terminal() {
        bail!(
            "no config found at {} -- run `riptune setup` in a terminal, or copy config.example.toml there",
            config_path.display()
        );
    }

    println!("\n{BOLD}riptune setup{RESET}");
    println!("Connect riptune to your Subsonic-compatible server (Navidrome, Airsonic, Gonic, ...).");
    println!("{DIM}Press Enter to keep a value in [brackets]. Ctrl-C to quit.{RESET}\n");

    if config_path.exists() && !confirm(&format!("A config already exists at {}. Replace it?", config_path.display()), false)? {
        println!("Kept the existing config.");
        return Ok(());
    }

    let mut answers = Answers::default();
    loop {
        let url_input = prompt("Server URL", Some(answers.url.as_str()).filter(|s| !s.is_empty()), Some("e.g. https://music.example.com"))?;
        answers.username = prompt("Username", Some(answers.username.as_str()).filter(|s| !s.is_empty()), None)?;
        let keep = (!answers.password.is_empty()).then_some("keep previous");
        let password = prompt_hidden("Password", keep)?;
        if !password.is_empty() {
            answers.password = password;
        }

        println!();
        let mut last_hint = String::new();
        let mut connected = false;
        for url in candidate_urls(&url_input) {
            answers.url = url;
            print!("Connecting to {}... ", answers.url);
            io::stdout().flush()?;
            match check(&answers).await {
                Ok(()) => {
                    println!("{GREEN}connected{RESET}");
                    connected = true;
                    break;
                }
                Err(hint) => {
                    println!("{RED}failed{RESET}");
                    last_hint = hint;
                }
            }
        }
        match connected {
            true => break,
            false => {
                println!("  {last_hint}\n");
                if !confirm("Try again?", true)? {
                    if confirm("Save these settings anyway?", false)? {
                        break;
                    }
                    bail!("setup cancelled; nothing was saved");
                }
                println!();
            }
        }
    }

    write_config(config_path, &answers)?;
    println!("Saved {} {DIM}(readable only by you){RESET}", config_path.display());
    println!("{DIM}Change colors or the cache location there; see config.example.toml.{RESET}\n");
    Ok(())
}

/// Pings the server and turns failures into advice the user can act on.
async fn check(a: &Answers) -> Result<(), String> {
    let client = SubsonicClient::new(&a.url, &a.username, &a.password).map_err(|e| format!("invalid URL: {e}"))?;
    client.ping().await.map_err(|e| match &e {
        SubsonicError::Api { code: 40, .. } => "Wrong username or password.".to_string(),
        SubsonicError::Api { code: 41, .. } => {
            "This server doesn't accept token auth for this account (common with LDAP users). \
             Try an API key or app password as the password."
                .to_string()
        }
        SubsonicError::Request(r) if r.is_connect() || r.is_timeout() => {
            format!("Couldn't reach the server. Check the URL and that the server is running. ({e})")
        }
        SubsonicError::Request(r) if r.is_decode() => {
            "Got a response, but it isn't from a Subsonic API. Use the address you open in the browser, \
             without /rest at the end."
                .to_string()
        }
        _ => e.to_string(),
    })
}

/// URLs to try for what the user typed. Strips a trailing `/rest` (people
/// often paste the API endpoint rather than the base URL). Without a scheme,
/// tries https first, then http, since home-network servers
/// (`192.168.1.10:4533`) usually speak plain http.
fn candidate_urls(input: &str) -> Vec<String> {
    let s = input.trim().trim_end_matches('/');
    let s = s.strip_suffix("/rest").unwrap_or(s);
    if s.contains("://") {
        vec![s.to_string()]
    } else {
        vec![format!("https://{s}"), format!("http://{s}")]
    }
}

fn render_config(a: &Answers) -> Result<String> {
    #[derive(serde::Serialize)]
    struct Server<'a> {
        url: &'a str,
        username: &'a str,
        password: &'a str,
    }
    #[derive(serde::Serialize)]
    struct File<'a> {
        server: Server<'a>,
    }
    let body = toml::to_string(&File { server: Server { url: &a.url, username: &a.username, password: &a.password } })?;
    Ok(format!(
        "# Written by `riptune setup`. Run it again to change these.\n\n{body}\n\
         # Optional, see config.example.toml:\n\
         # [cache]\n# path = \"~/.cache/riptune/library.sqlite\"\n\
         # [theme]\n# accent = \"#89b4fa\"\n"
    ))
}

fn write_config(path: &Path, a: &Answers) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path).with_context(|| format!("writing {}", path.display()))?;
    // mode() only applies on creation; also tighten a pre-existing file.
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    file.write_all(render_config(a)?.as_bytes())?;
    Ok(())
}

fn prompt(label: &str, default: Option<&str>, example: Option<&str>) -> Result<String> {
    loop {
        match (default, example) {
            (Some(d), _) => print!("{BOLD}{label}{RESET} [{d}]: "),
            (None, Some(e)) => print!("{BOLD}{label}{RESET} {DIM}({e}){RESET}: "),
            (None, None) => print!("{BOLD}{label}{RESET}: "),
        }
        io::stdout().flush()?;
        let mut line = String::new();
        if io::stdin().lock().read_line(&mut line)? == 0 {
            bail!("setup cancelled");
        }
        let line = line.trim();
        match (line.is_empty(), default) {
            (false, _) => return Ok(line.to_string()),
            (true, Some(d)) => return Ok(d.to_string()),
            (true, None) => println!("  {DIM}required{RESET}"),
        }
    }
}

fn confirm(question: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "Y/n" } else { "y/N" };
    print!("{question} [{hint}] ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(match line.trim().to_lowercase().as_str() {
        "" => default_yes,
        s => s.starts_with('y'),
    })
}

/// Reads a password without echoing it, showing `•` per character. Empty
/// input is allowed only when `keep_hint` says what an empty answer means.
fn prompt_hidden(label: &str, keep_hint: Option<&str>) -> Result<String> {
    loop {
        match keep_hint {
            Some(h) => print!("{BOLD}{label}{RESET} [{h}]: "),
            None => print!("{BOLD}{label}{RESET} {DIM}(or API key; hidden){RESET}: "),
        }
        io::stdout().flush()?;
        crossterm::terminal::enable_raw_mode()?;
        let result = read_hidden();
        crossterm::terminal::disable_raw_mode()?;
        println!();
        let password = result?;
        if !password.is_empty() || keep_hint.is_some() {
            return Ok(password);
        }
        println!("  {DIM}required{RESET}");
    }
}

fn read_hidden() -> Result<String> {
    let mut out = String::new();
    loop {
        let Event::Key(key) = event::read()? else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Enter => return Ok(out),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => bail!("setup cancelled"),
            KeyCode::Esc => bail!("setup cancelled"),
            KeyCode::Backspace => {
                if out.pop().is_some() {
                    print!("\x08 \x08");
                }
            }
            KeyCode::Char(c) => {
                out.push(c);
                print!("•");
            }
            _ => continue,
        }
        io::stdout().flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_common_url_mistakes() {
        assert_eq!(candidate_urls("music.example.com"), ["https://music.example.com", "http://music.example.com"]);
        assert_eq!(candidate_urls(" http://10.0.0.2:4533/ "), ["http://10.0.0.2:4533"]);
        assert_eq!(candidate_urls("https://host/navidrome/rest/"), ["https://host/navidrome"]);
    }

    #[test]
    fn written_config_round_trips_through_the_loader() {
        let a = Answers {
            url: "https://music.example.com".into(),
            username: "alice".into(),
            password: "p\"a#ss\\word".into(),
        };
        let cfg: crate::config::Config = toml::from_str(&render_config(&a).unwrap()).unwrap();
        assert_eq!(cfg.server.url, a.url);
        assert_eq!(cfg.server.username, a.username);
        assert_eq!(cfg.server.password, a.password);
    }
}
