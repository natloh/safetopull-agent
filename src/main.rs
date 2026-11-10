use std::process::ExitCode;
use std::time::Duration;

use stp_agent::app::App;
use stp_agent::config::{Config, UpdateMode};
use stp_agent::host::HostFacts;
use stp_agent::web;

#[tokio::main]
async fn main() -> ExitCode {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => return fail(&error),
    };

    match std::env::args().nth(1).as_deref() {
        Some("--version") => {
            println!("stp-agent {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("healthcheck") => return healthcheck(&config).await,
        Some(other) => {
            return fail(&format!(
                "unknown argument `{other}`; expected --version or healthcheck"
            ))
        }
        None => {}
    }

    let app = match App::start(config, HostFacts::detect()).await {
        Ok(app) => app,
        Err(error) => return fail(&error),
    };

    let listener = match tokio::net::TcpListener::bind(app.config.listen).await {
        Ok(listener) => listener,
        Err(error) => return fail(&format!("cannot listen on {}: {error}", app.config.listen)),
    };

    eprintln!(
        "stp-agent {} listening on {} ({} mode, remote actions {})",
        env!("CARGO_PKG_VERSION"),
        app.config.listen,
        if app.config.update_mode == UpdateMode::Apply {
            "apply"
        } else {
            "advisory"
        },
        if app.config.remote_actions { "on" } else { "off" },
    );
    if app.config.update_mode == UpdateMode::Apply && app.config.api_token.is_none() {
        eprintln!("stp-agent: apply mode without STP_API_TOKEN lets anyone who can reach this port start updates");
    }

    let background = app.clone();
    tokio::spawn(async move {
        loop {
            background.cycle().await;
            tokio::time::sleep(background.config.scan_interval).await;
        }
    });

    match axum::serve(listener, web::router(app))
        .with_graceful_shutdown(shutdown())
        .await
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error),
    }
}

fn fail(error: &dyn std::fmt::Display) -> ExitCode {
    eprintln!("stp-agent: {error}");
    ExitCode::FAILURE
}

async fn healthcheck(config: &Config) -> ExitCode {
    let url = format!("http://127.0.0.1:{}/healthz", config.listen.port());
    let healthy = match reqwest::Client::builder().timeout(Duration::from_secs(5)).build() {
        Ok(client) => client
            .get(url)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success()),
        Err(_) => false,
    };

    if healthy {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

async fn shutdown() {
    let interrupt = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("a SIGTERM handler can be installed");
        tokio::select! {
            _ = interrupt => {}
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    let _ = interrupt.await;
}
