use psi::{
    core::{Plugin, Runtime},
    plugins::{BufferedResponsePlugin, FixtureModelPlugin, SingleResponsePlugin, StartupProbe},
    protocol::Controller,
};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut selection = "single-response".to_string();
    let mut gate = None;
    let mut fault = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().unwrap_or_default();
        match arg.as_str() {
            "--agent-loop" => selection = value,
            "--fixture-gate" => gate = Some(std::path::PathBuf::from(value)),
            "--startup-fault" => fault = Some(value),
            _ => {
                eprintln!("unknown option: {arg}");
                std::process::exit(1);
            }
        }
    }
    let agent: Box<dyn Plugin> = match selection.as_str() {
        "single-response" => Box::new(SingleResponsePlugin),
        "buffered-response" => Box::new(BufferedResponsePlugin),
        _ => {
            eprintln!("unknown agent loop: {selection}");
            std::process::exit(1);
        }
    };
    let mut plugins: Vec<Box<dyn Plugin>> = vec![agent, Box::new(FixtureModelPlugin { gate })];
    if let Some(fault) = fault {
        let fault = match fault.as_str() {
            "missing" => "missing",
            "cycle" => "cycle",
            "duplicate" => "duplicate",
            "partial" => "partial",
            _ => {
                eprintln!("unknown startup fault");
                std::process::exit(1);
            }
        };
        plugins.push(Box::new(StartupProbe(fault)));
    }
    let mut runtime = match Runtime::start(plugins).await {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Psi startup failed: {error}");
            std::process::exit(1);
        }
    };
    eprintln!("Psi fixture runtime started");
    let result = match runtime.service("agent-loop") {
        Ok(agent) => {
            Controller::new(
                agent,
                runtime.work(),
                runtime.spawner("agent-loop").unwrap(),
            )
            .serve()
            .await
        }
        Err(error) => Err(error),
    };
    let cleanup = runtime.close().await;
    eprintln!("Psi runtime stopped");
    if let Err(error) = &result {
        eprintln!("Psi transport failed: {error}");
    }
    if let Err(error) = &cleanup {
        eprintln!("Psi shutdown failed: {error}");
    }
    if result.is_err() || cleanup.is_err() {
        std::process::exit(1);
    }
}
