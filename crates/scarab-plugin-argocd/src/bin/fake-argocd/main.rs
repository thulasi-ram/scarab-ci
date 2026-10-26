//! `fake-argocd`: the fake Argo CD API server ([`server`]) as a process, for
//! `just plugin-verify` (on the host, behind the proc-mode broker) and the
//! image's `fake` target (in-cluster). Never part of the reporter image.
//!
//! Usage: `fake-argocd [--addr 127.0.0.1:18080]`. `FAKE_ARGOCD_TOKEN`, when
//! set, is the bearer token Argo's routes require — what the admin binds as
//! the PluginConnection credential, so a request that did not come through
//! the broker (or came without the credential) is refused.

mod server;

#[tokio::main]
async fn main() {
    let mut addr = "127.0.0.1:18080".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--addr" => match args.next() {
                Some(a) => addr = a,
                None => fail("--addr needs a value"),
            },
            "-h" | "--help" => {
                println!("usage: fake-argocd [--addr HOST:PORT]  (env FAKE_ARGOCD_TOKEN)");
                return;
            }
            other => fail(&format!("unknown argument {other:?}")),
        }
    }
    let token = std::env::var("FAKE_ARGOCD_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    let fake = server::FakeArgo::new(token.clone());
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => fail(&format!("bind {addr}: {e}")),
    };
    println!(
        "fake-argocd listening on http://{addr} (bearer token {})",
        if token.is_some() {
            "required"
        } else {
            "not checked"
        }
    );
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    if let Err(e) = axum::serve(listener, server::router(fake))
        .with_graceful_shutdown(shutdown)
        .await
    {
        fail(&format!("serve: {e}"));
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("fake-argocd: {msg}");
    std::process::exit(2);
}
