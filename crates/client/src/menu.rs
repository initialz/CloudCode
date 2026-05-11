//! Cooked-mode workspace selection menu shown before opening a PTY.
//!
//! Run a workspace list against the currently selected agent, print a
//! numbered menu, read one line from stdin, and translate it into a
//! `MenuAction`. The caller (main.rs) drives the menu → relay → menu loop.

use crate::proto::{ClientToHub, HubToClient};
use crate::wire::{OutFrame, Wire};
use anyhow::{anyhow, Result};
use tokio::sync::mpsc;

pub enum MenuAction {
    /// Open a PTY in this workspace.
    Open(String),
    /// Switch to a different agent (None = let hub pick) and re-enter menu.
    SwitchAgent(Option<String>),
    /// Quit cloudcode.
    Quit,
}

pub async fn run(
    wire: &mut Wire,
    stdin_rx: &mut mpsc::Receiver<Vec<u8>>,
    current_agent: &str,
    default_workspace: Option<&str>,
) -> Result<MenuAction> {
    loop {
        let workspaces = list_workspaces(wire).await?;
        let default_idx = default_workspace.and_then(|w| workspaces.iter().position(|x| x == w));

        print_menu(current_agent, &workspaces, default_idx);

        let line = match read_line(stdin_rx).await {
            Some(s) => s,
            None => return Ok(MenuAction::Quit),
        };
        let line = line.trim();

        // Empty → pick default if any
        if line.is_empty() {
            if let Some(idx) = default_idx {
                return Ok(MenuAction::Open(workspaces[idx].clone()));
            } else {
                continue;
            }
        }

        let mut parts = line.splitn(2, char::is_whitespace);
        let head = parts.next().unwrap_or("");
        let rest = parts.next().map(|s| s.trim()).unwrap_or("");

        match head {
            "q" | "quit" | "exit" => return Ok(MenuAction::Quit),
            "c" => {
                if rest.is_empty() {
                    eprintln!("usage: c <name>");
                    continue;
                }
                create_workspace(wire, rest).await?;
            }
            "d" => {
                if rest.is_empty() {
                    eprintln!("usage: d <num|name>");
                    continue;
                }
                let name = resolve(&workspaces, rest);
                let Some(name) = name else {
                    eprintln!("not found: {}", rest);
                    continue;
                };
                delete_workspace(wire, &name).await?;
            }
            "a" => {
                if rest.is_empty() {
                    list_agents(wire, current_agent).await?;
                } else {
                    return Ok(MenuAction::SwitchAgent(Some(rest.into())));
                }
            }
            _ => {
                // try as number or name
                if let Some(name) = resolve(&workspaces, line) {
                    return Ok(MenuAction::Open(name));
                } else {
                    eprintln!("not found: {}", line);
                }
            }
        }
    }
}

fn print_menu(agent: &str, workspaces: &[String], default_idx: Option<usize>) {
    println!();
    println!("agent: {}", agent);
    if workspaces.is_empty() {
        println!("workspaces: (none — `c <name>` to create one)");
    } else {
        println!("workspaces:");
        for (i, w) in workspaces.iter().enumerate() {
            let star = if Some(i) == default_idx { " *" } else { "  " };
            println!("  [{}]{}{}", i + 1, star, w);
        }
    }
    println!();
    println!("  c <name>   create workspace        d <num|name>   delete workspace");
    println!("  a [name]   list/switch agent         q              quit");
    let prompt = match default_idx {
        Some(i) => format!("choose [{}]: ", i + 1),
        None => "choose: ".to_string(),
    };
    print!("{}", prompt);
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn resolve(workspaces: &[String], input: &str) -> Option<String> {
    if let Ok(n) = input.parse::<usize>() {
        if n >= 1 && n <= workspaces.len() {
            return Some(workspaces[n - 1].clone());
        }
        return None;
    }
    if workspaces.iter().any(|w| w == input) {
        return Some(input.to_string());
    }
    None
}

async fn list_workspaces(wire: &mut Wire) -> Result<Vec<String>> {
    wire.out_tx
        .send(OutFrame::Text(ClientToHub::ListWorkspaces))
        .await
        .map_err(|_| anyhow!("hub disconnected"))?;
    loop {
        let m = wire
            .in_text_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("hub disconnected"))?;
        match m {
            HubToClient::WorkspaceList { items } => return Ok(items),
            HubToClient::SessionError { message } => {
                eprintln!("[cc] {}", message);
                return Ok(Vec::new());
            }
            HubToClient::Ping => {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
            }
            _ => continue,
        }
    }
}

async fn create_workspace(wire: &mut Wire, name: &str) -> Result<()> {
    wire.out_tx
        .send(OutFrame::Text(ClientToHub::CreateWorkspace {
            name: name.into(),
        }))
        .await
        .map_err(|_| anyhow!("hub disconnected"))?;
    loop {
        let m = wire
            .in_text_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("hub disconnected"))?;
        match m {
            HubToClient::WorkspaceCreated { name } => {
                println!("[cc] workspace '{}' created", name);
                return Ok(());
            }
            HubToClient::SessionError { message } => {
                eprintln!("[cc] {}", message);
                return Ok(());
            }
            HubToClient::Ping => {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
            }
            _ => continue,
        }
    }
}

async fn delete_workspace(wire: &mut Wire, name: &str) -> Result<()> {
    wire.out_tx
        .send(OutFrame::Text(ClientToHub::DeleteWorkspace {
            name: name.into(),
        }))
        .await
        .map_err(|_| anyhow!("hub disconnected"))?;
    loop {
        let m = wire
            .in_text_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("hub disconnected"))?;
        match m {
            HubToClient::WorkspaceDeleted { name } => {
                println!("[cc] workspace '{}' deleted", name);
                return Ok(());
            }
            HubToClient::SessionError { message } => {
                eprintln!("[cc] {}", message);
                return Ok(());
            }
            HubToClient::Ping => {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
            }
            _ => continue,
        }
    }
}

async fn list_agents(wire: &mut Wire, current_agent: &str) -> Result<()> {
    wire.out_tx
        .send(OutFrame::Text(ClientToHub::ListAgents))
        .await
        .map_err(|_| anyhow!("hub disconnected"))?;
    loop {
        let m = wire
            .in_text_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("hub disconnected"))?;
        match m {
            HubToClient::AgentList { items } => {
                if items.is_empty() {
                    println!("[cc] no agents online");
                } else {
                    println!("[cc] agents:");
                    for it in &items {
                        let star = if it.name == current_agent { " *" } else { "  " };
                        println!("    {}{}", star, it.name);
                    }
                    println!("[cc] (use `a <name>` to switch)");
                }
                return Ok(());
            }
            HubToClient::SessionError { message } => {
                eprintln!("[cc] {}", message);
                return Ok(());
            }
            HubToClient::Ping => {
                let _ = wire.out_tx.send(OutFrame::Text(ClientToHub::Pong)).await;
            }
            _ => continue,
        }
    }
}

/// Read one line from the shared stdin pump. cooked mode delivers one line
/// (terminated by '\n') per recv, so this is usually a single iteration.
async fn read_line(stdin_rx: &mut mpsc::Receiver<Vec<u8>>) -> Option<String> {
    let mut buf = Vec::new();
    loop {
        let chunk = stdin_rx.recv().await?;
        buf.extend_from_slice(&chunk);
        if buf.contains(&b'\n') {
            break;
        }
    }
    let s = String::from_utf8_lossy(&buf).into_owned();
    Some(s.trim_end_matches(['\r', '\n']).to_string())
}
