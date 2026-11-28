//! `expanse-kv` binary entry point (`docs/PRD.md` §7 Phase 3).

#![forbid(unsafe_code)]

use std::env;
use std::process::ExitCode;

use expanse_kv::config::{CliAction, USAGE, parse_cli_args};
use expanse_kv::server::RunningServer;
use tokio::sync::watch;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let action = match parse_cli_args(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    match action {
        CliAction::Help => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        CliAction::Version => {
            println!("expanse-kv {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        CliAction::Serve(cfg) => {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    eprintln!("error: failed to build tokio runtime: {e}");
                    return ExitCode::from(1);
                }
            };

            rt.block_on(async move {
                let server = match RunningServer::bind(&cfg).await {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::from(1);
                    }
                };
                eprintln!("expanse-kv listening on {}", server.local_addr());
                if let Some(ma) = server.metrics_addr() {
                    eprintln!("expanse-kv metrics listening on http://{ma}/metrics");
                }

                let (tx, rx) = watch::channel(false);
                tokio::spawn(async move {
                    if tokio::signal::ctrl_c().await.is_ok() {
                        let _sent = tx.send(true);
                    }
                });

                if let Err(e) = server.serve(rx).await {
                    eprintln!("error: {e}");
                    return ExitCode::from(1);
                }
                ExitCode::SUCCESS
            })
        }
    }
}
