use psi::{
    core::Runtime,
    plugins::{FixtureModelPlugin, SingleResponsePlugin},
    protocol::Controller,
};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut runtime = match Runtime::start(vec![
        Box::new(SingleResponsePlugin),
        Box::new(FixtureModelPlugin),
    ])
    .await
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("Psi startup failed: {error}");
            std::process::exit(1);
        }
    };
    eprintln!("Psi fixture runtime started");
    let result = match runtime.service("agent-loop") {
        Ok(agent) => Controller::new(agent).serve().await,
        Err(error) => Err(error),
    };
    runtime.close().await;
    eprintln!("Psi runtime stopped");
    if let Err(error) = result {
        eprintln!("Psi transport failed: {error}");
        std::process::exit(1);
    }
}
