use clap::Parser;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
#[derive(Parser)]
#[command(name=proto::names::RELAY_BIN,version,about="Loopback API relay; no bodies or credentials are stored")]
struct Args {
    /// One fixed API origin/base; HTTPS except literal loopback test servers.
    #[arg(long)]
    upstream: String,
    #[arg(long, default_value = "127.0.0.1:14599")]
    listen: SocketAddr,
    /// auto follows the daemon; normal streams; away buffers complete replies.
    #[arg(long,default_value="auto",value_parser=["auto","normal","away"])]
    mode: String,
    /// Explicit proxy URL, or direct. By default follow system/HTTPS_PROXY.
    #[arg(long)]
    proxy: Option<String>,
    #[arg(long,default_value_t=64,value_parser=clap::value_parser!(u32).range(1..=256))]
    buffer_mb: u32,
    #[arg(long,default_value_t=3,value_parser=clap::value_parser!(u32).range(1..=3))]
    attempts: u32,
    #[arg(long,env=proto::names::ENV_ENDPOINT)]
    endpoint: Option<String>,
    #[arg(long, hide = true)]
    worker: bool,
}
fn watch_mode(buffer: Arc<AtomicBool>, endpoint: Option<String>) {
    std::thread::spawn(move || {
        let Ok(endpoint) = endpoint.map_or_else(platform::paths::default_endpoint, Ok) else {
            return;
        };
        loop {
            let client = proto::ClientInfo {
                name: proto::names::RELAY_BIN.into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            };
            if let Ok(mut client) = platform::connect_client(&endpoint, client, Duration::from_millis(250)) {
                if let Ok(reply) = client.call("mode.show", serde_json::json!({})) {
                    if let Some(result) = reply.result {
                        if let Some(mode) = result["mode"].as_str() {
                            buffer.store(mode != "normal", Ordering::SeqCst);
                        }
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    let args = Args::parse();
    let validation = relay::Config::new(args.listen, &args.upstream).and_then(|mut c| {
        c.max_buffer_bytes = args.buffer_mb as usize * 1024 * 1024;
        c.attempts = args.attempts;
        c.proxy = args.proxy.clone();
        c.validate()
    });
    if let Err(error) = validation {
        eprintln!("relay: {error}");
        return std::process::ExitCode::FAILURE;
    }
    let result = if args.worker { worker(args).await } else { supervise().await };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("relay: {message}");
            std::process::ExitCode::FAILURE
        }
    }
}
async fn worker(args: Args) -> Result<(), String> {
    let mut config = relay::Config::new(args.listen, &args.upstream)?;
    config.max_buffer_bytes = args.buffer_mb as usize * 1024 * 1024;
    config.attempts = args.attempts;
    config.proxy = args.proxy;
    let buffer = Arc::new(AtomicBool::new(args.mode == "away"));
    if args.mode == "auto" {
        watch_mode(buffer.clone(), args.endpoint);
    }
    let relay = relay::Relay::new(config, buffer)?;
    let listener = tokio::net::TcpListener::bind(args.listen).await.map_err(|e| e.to_string())?;
    eprintln!("relay listening on {} (worker {})", args.listen, std::process::id());
    relay
        .serve(listener, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| e.to_string())
}
async fn supervise() -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    loop {
        let mut command = tokio::process::Command::new(&executable);
        command.args(&arguments).arg("--worker").kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let mut child = command.spawn().map_err(|e| e.to_string())?;
        tokio::select! {result=child.wait()=>{let _=result.map_err(|e|e.to_string())?;},_=tokio::signal::ctrl_c()=>{let _=child.kill().await;return Ok(());}}
        tokio::select! {_=tokio::time::sleep(Duration::from_secs(1))=>{},_=tokio::signal::ctrl_c()=>return Ok(())}
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn binary_name_matches_names_table() {
        assert_eq!(env!("CARGO_BIN_NAME"), proto::names::RELAY_BIN);
    }
}
