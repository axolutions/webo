use std::io::{IsTerminal, Read};
use std::process::ExitCode;
use webo::config::Server;
use webo::{Failure, Io, Transport, fail, mcp, run};

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let config = std::env::var("HOME")
        .ok()
        .and_then(|home| std::fs::read_to_string(format!("{home}/.config/webo/config")).ok());
    let var = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    let read_input = |path: &str| -> Result<String, Failure> {
        if path == "-" {
            let mut text = String::new();
            std::io::stdin()
                .read_to_string(&mut text)
                .map_err(|e| fail("input", format!("stdin: {e}")))?;
            return Ok(text);
        }
        std::fs::read_to_string(path).map_err(|e| fail("input", format!("{path}: {e}")))
    };
    let connect = |server: &Server| -> Box<dyn Transport> { Box::new(mcp::Http::new(server)) };
    let io = Io {
        config,
        var: &var,
        read_input: &read_input,
        stdin_piped: !std::io::stdin().is_terminal(),
        connect: &connect,
    };
    let outcome = run(&argv, &io);
    println!("{}", outcome.stdout.trim_end());
    ExitCode::from(outcome.code)
}
