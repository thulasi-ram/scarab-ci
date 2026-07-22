//! OpenAPI export acceptance (ADR-0012, 0028): the generated document carries
//! the /v1 paths + IR-subset schemas, and `scarab-server --emit-openapi` writes
//! it to a file for client codegen / CI spec checks.

#[test]
fn openapi_document_has_v1_paths_and_ir_schemas() {
    let doc: serde_json::Value = serde_json::from_str(&scarab_server::openapi_json()).unwrap();

    let paths = &doc["paths"];
    for p in [
        "/v1/runs",
        "/v1/runs/{id}",
        "/v1/runs/{id}/events",
        "/v1/runs/{id}/logs",
        "/v1/runs/{id}/steps/{step}/rerun",
        "/v1/runs/{id}/gates/{step}/approve",
    ] {
        assert!(paths.get(p).is_some(), "missing path {p}");
    }

    let schemas = &doc["components"]["schemas"];
    for s in [
        "CreateRunRequest",
        "PipelineDto",
        "StepDto",
        "CreateRunResponse",
        "RunStatusResponse",
    ] {
        assert!(schemas.get(s).is_some(), "missing schema {s}");
    }
}

#[test]
fn cli_emits_openapi_json_file() {
    let dir = std::env::temp_dir().join(format!("scarab-openapi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("openapi.json");

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_scarab-server"))
        .arg("--emit-openapi")
        .arg(&path)
        .status()
        .expect("run scarab-server");
    assert!(status.success());

    let content = std::fs::read_to_string(&path).expect("openapi.json written");
    let doc: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
    assert!(doc["paths"].get("/v1/runs").is_some());
    assert_eq!(
        doc["openapi"].as_str().map(|s| s.starts_with("3.")),
        Some(true)
    );

    let _ = std::fs::remove_dir_all(&dir);
}
