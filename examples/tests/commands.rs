use std::process::Command;

#[test]
fn all_programs_explain_missing_configuration_without_a_network_call() {
    let output = Command::new(env!("CARGO_BIN_EXE_tap-seller"))
        .env_remove("PUBLIC_ORIGIN")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Set PUBLIC_ORIGIN; see examples/README.md.\n"
    );
    for binary in [
        env!("CARGO_BIN_EXE_mpp-buyer"),
        env!("CARGO_BIN_EXE_mpp-seller"),
        env!("CARGO_BIN_EXE_x402-buyer"),
        env!("CARGO_BIN_EXE_x402-seller"),
    ] {
        let output = Command::new(binary)
            .env_remove("INFLOW_API_KEY")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "Set INFLOW_API_KEY; see examples/README.md.\n"
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_mpp-seller"))
        .env("INFLOW_API_KEY", "not-sent")
        .env_remove("MPP_SECRET_KEY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("Set MPP_SECRET_KEY")
    );
    for target in ["not a url", "https://user:secret@example.com/"] {
        let output = Command::new(env!("CARGO_BIN_EXE_x402-buyer"))
            .env("INFLOW_API_KEY", "not-sent")
            .env("TARGET_URL", target)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(!error.contains("not-sent"));
        assert!(!error.contains("secret"));
    }
}

#[tokio::test]
async fn both_buyer_programs_can_read_an_unpaid_resource_without_platform_access() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/free", listener.local_addr().unwrap());
    let app = axum::Router::new().route("/free", axum::routing::get(|| async { "free resource" }));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    for binary in [
        env!("CARGO_BIN_EXE_mpp-buyer"),
        env!("CARGO_BIN_EXE_x402-buyer"),
    ] {
        let url = url.clone();
        let output = tokio::task::spawn_blocking(move || {
            Command::new(binary)
                .env("INFLOW_API_KEY", "not-sent")
                .env("TARGET_URL", url)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let out = String::from_utf8(output.stdout).unwrap();
        assert!(out.contains("free resource"));
        assert!(out.contains("No payment was initiated"));
        assert!(!out.contains("not-sent"));
    }
    server.abort();
}
