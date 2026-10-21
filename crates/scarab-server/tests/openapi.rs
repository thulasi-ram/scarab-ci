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

/// The Deployment record's read API (ADR-0070) is on the documented surface,
/// with typed bodies the web UI generates its client from, and grouped apart
/// from the repo-scoped legacy approval history that shares the word.
#[test]
fn openapi_document_has_the_deployment_record() {
    let doc: serde_json::Value = serde_json::from_str(&scarab_server::openapi_json()).unwrap();

    let paths = &doc["paths"];
    for p in [
        "/v1/orgs/{org}/board",
        "/v1/orgs/{org}/environments/{env}/deployments",
        "/v1/orgs/{org}/environments/{env}/deployments/{id}",
    ] {
        let op = &paths[p]["get"];
        assert!(op.is_object(), "missing GET {p}");
        assert_eq!(op["tags"], serde_json::json!(["Deployments"]), "{p}");
    }
    assert_eq!(
        paths["/v1/repos/{org}/{repo}/environments/{name}/deployments"]["get"]["tags"],
        serde_json::json!(["Environments"]),
        "the legacy approval history stays with Environments"
    );

    let schemas = &doc["components"]["schemas"];
    for s in [
        "DeploymentDto",
        "DeploymentStatusDto",
        "SourceAssertedStatusDto",
        "DeploymentTargetDto",
        "DeploymentRequesterDto",
        "DeploymentStatusReportDto",
        "EnvironmentBoardRowDto",
    ] {
        assert!(schemas.get(s).is_some(), "missing schema {s}");
    }
    assert_eq!(
        schemas["DeploymentStatusDto"]["enum"],
        serde_json::json!([
            "requested",
            "in_progress",
            "live",
            "failed",
            "stalled",
            "superseded"
        ])
    );
}

/// Org-level Environments (ADR-0071) are on the documented surface with typed
/// bodies, grouped with Environments — not with the Deployment record that
/// shares their path prefix.
#[test]
fn openapi_document_has_org_environments() {
    let doc: serde_json::Value = serde_json::from_str(&scarab_server::openapi_json()).unwrap();

    let paths = &doc["paths"];
    for (p, method) in [
        ("/v1/orgs/{org}/environments", "get"),
        ("/v1/orgs/{org}/environments/{env}", "get"),
        ("/v1/orgs/{org}/environments/{env}", "put"),
        ("/v1/orgs/{org}/environments/{env}", "delete"),
    ] {
        let op = &paths[p][method];
        assert!(op.is_object(), "missing {method} {p}");
        assert_eq!(
            op["tags"],
            serde_json::json!(["Environments"]),
            "{method} {p}"
        );
    }
    assert_eq!(
        paths["/v1/orgs/{org}/environments/{env}"]["put"]["requestBody"]["content"]
            ["application/json"]["schema"]["$ref"],
        serde_json::json!("#/components/schemas/PutOrgEnvironmentRequest")
    );

    let schemas = &doc["components"]["schemas"];
    for s in [
        "OrgEnvironmentDto",
        "EnvironmentDeploymentSourceDto",
        "PutOrgEnvironmentRequest",
    ] {
        assert!(schemas.get(s).is_some(), "missing schema {s}");
    }
    // A full replace: the source list is the one field a caller must state.
    assert_eq!(
        schemas["PutOrgEnvironmentRequest"]["required"],
        serde_json::json!(["deployment_sources"])
    );
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

/// The plugin admin API (ADR-0072) is on the documented surface under its
/// own group, with typed bodies; the broker's plugin-facing routes are not —
/// they live on a separate listener, never the public API.
#[test]
fn openapi_document_has_the_plugin_admin_api_but_not_the_broker() {
    let doc: serde_json::Value = serde_json::from_str(&scarab_server::openapi_json()).unwrap();

    let paths = &doc["paths"];
    for (p, method) in [
        ("/v1/orgs/{org}/plugin-catalog", "get"),
        ("/v1/orgs/{org}/plugins", "get"),
        ("/v1/orgs/{org}/plugins", "post"),
        ("/v1/orgs/{org}/plugins/{name}", "get"),
        ("/v1/orgs/{org}/plugins/{name}", "delete"),
        ("/v1/orgs/{org}/plugins/{name}/kill", "post"),
        ("/v1/orgs/{org}/plugins/{name}/enable", "post"),
        ("/v1/orgs/{org}/plugins/{name}/unmapped-groupings", "get"),
    ] {
        let op = &paths[p][method];
        assert!(op.is_object(), "missing {method} {p}");
        assert_eq!(op["tags"], serde_json::json!(["Plugins"]), "{method} {p}");
    }
    assert_eq!(
        paths["/v1/orgs/{org}/environments/{env}/deployment-targets"]["get"]["tags"],
        serde_json::json!(["Deployments"])
    );
    assert!(
        paths
            .as_object()
            .unwrap()
            .keys()
            .all(|p| !p.starts_with("/_scarab/")),
        "the broker is not part of the public API"
    );

    let schemas = &doc["components"]["schemas"];
    for s in [
        "PluginInstallationDto",
        "InstallPluginRequest",
        "PluginCatalogEntryDto",
        "PluginManifestDto",
        "PluginCapabilityDto",
        "UninstalledPluginDto",
        "UnmappedGroupingDto",
        "DeploymentSourceTargetDto",
    ] {
        assert!(schemas.get(s).is_some(), "missing schema {s}");
    }
    assert_eq!(
        schemas["PluginCapabilityDto"]["enum"],
        serde_json::json!([
            "report_deployments",
            "subscribe_events",
            "offer_actions",
            "use_connection",
            "contribute_ui",
            "approve_gates"
        ])
    );
}
