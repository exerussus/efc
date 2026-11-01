//! `efc __helper …` — скрытая команда для сквозных тестов: заменяет `echo`, `sleep`, `true`, `false`, `sh`,
//! чтобы тесты одинаково шли на Windows, Linux и macOS.

use std::process::{Command, Stdio};
use std::time::Duration;

pub fn run(args: &[String]) -> i32 {
    let secs = |i: usize| args.get(i).and_then(|a| a.parse::<f64>().ok()).unwrap_or(1.0);
    match args.first().map(String::as_str) {
        // повторить аргументы
        Some("echo") => {
            println!("{}", args[1..].join(" "));
            0
        }
        // поспать N секунд
        Some("sleep") => {
            std::thread::sleep(Duration::from_secs_f64(secs(1)));
            0
        }
        // выйти с кодом
        Some("exit") => args.get(1).and_then(|a| a.parse().ok()).unwrap_or(1),
        // внук держит вывод: запустить «sleep N» с общим stdout, дождаться его, напечатать x
        Some("deep") => {
            let me = std::env::current_exe().expect("путь к efc");
            let st = Command::new(me).args(["__helper", "sleep", &secs(1).to_string()]).status();
            println!("x");
            if st.map(|s| s.success()).unwrap_or(false) {
                0
            } else {
                1
            }
        }
        // фоновый внук: запустить «sleep N» с общим stdout и выйти сразу
        Some("bg") => {
            let me = std::env::current_exe().expect("путь к efc");
            let _ = Command::new(me).args(["__helper", "sleep", &secs(1).to_string()]).stdin(Stdio::null()).spawn();
            println!("started");
            0
        }
        _ => {
            eprintln!("__helper: echo … | sleep <с> | exit <код> | deep <с> | bg <с>");
            2
        }
    }
}
