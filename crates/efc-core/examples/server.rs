//! Минимальный сервер: `cargo run -p efc-core --example server -- <папка канала>`,
//! затем в другом окне `efc --channel <папка> call build`.

use efc_core::model::{CommandInfo, Op};
use efc_core::server::{Ctx, Handler, Outcome, Server, ServerConfig};
use std::sync::Arc;
use std::time::Duration;

struct App;

impl Handler for App {
    fn commands(&self) -> Vec<CommandInfo> {
        vec![CommandInfo::new("build", "Собрать проект (пример: 2 секунды)", "build [--release]").long()]
    }

    fn run(&self, ctx: &Ctx, op: &Op) -> Outcome {
        let release = op.args.iter().any(|a| a == "--release");
        for i in 0..20 {
            if ctx.cancelled() {
                return Outcome::cancelled();
            }
            ctx.set_phase("компиляция", Some(i as f64 / 20.0));
            std::thread::sleep(Duration::from_millis(100));
        }
        Outcome::ok(format!("собрано{}\n", if release { " (release)" } else { "" }))
    }
}

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| "./.efc".into());
    let mut cfg = ServerConfig::new(&root, "example", "1.0.0");
    cfg.how_to_start = format!("cargo run -p efc-core --example server -- {root}");
    let server = Server::start(cfg, Arc::new(App)).expect("канал");
    eprintln!("канал {} обслуживается; Enter — остановить", server.layout().root.display());
    let _ = std::io::stdin().read_line(&mut String::new());
    server.stop(Duration::from_secs(5));
}
