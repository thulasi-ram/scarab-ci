//! The one route list of the main API router (ADR-0073 §3; "The guard" in
//! its Implementation notes).
//!
//! Every route is registered through [`Routes`], which takes the route's
//! [`Op`] classification as an argument: a GET is [`ops::READ`]; a POST, PUT
//! or DELETE cannot be registered without naming its `&'static Op`. So a new
//! mutating route cannot exist without saying whether it is a crown jewel,
//! and [`route_table`] is the complete, test-enumerable list the step-up
//! guard consults at runtime. A mutating request the table does not know is
//! refused 500 by the guard (fail-closed).
//!
//! There is deliberately no `.any(...)`, no `.route(...)` escape hatch and no
//! nesting: `tests/step_up_routes.rs` scans the sources for `.route(` outside
//! this file.

use std::sync::OnceLock;

use axum::handler::Handler;
use axum::http::Method;
use axum::routing::MethodRouter;
use axum::Router;

use crate::step_up::ops::{self, Op};
use crate::{org_environments as oe, AppState};

/// One registered (method, path) and how it is governed.
#[derive(Debug, Clone)]
pub struct RouteEntry {
    pub method: Method,
    /// The axum path pattern (`/v1/orgs/{org}/bindings`) — what
    /// `MatchedPath` reports for a request.
    pub path: &'static str,
    pub op: &'static Op,
}

/// The router under construction plus its classification table, built in
/// lockstep so neither can have an entry the other lacks.
pub(crate) struct Routes {
    pub(crate) router: Router<AppState>,
    pub(crate) table: Vec<RouteEntry>,
}

impl Routes {
    fn new() -> Self {
        Self {
            router: Router::new(),
            table: Vec::new(),
        }
    }

    fn add(
        mut self,
        method: Method,
        path: &'static str,
        op: &'static Op,
        mr: MethodRouter<AppState>,
    ) -> Self {
        // axum merges method routers registered on the same path, and panics
        // on a duplicate (path, method) — so the table cannot hold one either.
        self.router = self.router.route(path, mr);
        self.table.push(RouteEntry { method, path, op });
        self
    }

    /// A read: always [`ops::READ`].
    fn get<H, T>(self, path: &'static str, h: H) -> Self
    where
        H: Handler<T, AppState>,
        T: 'static,
    {
        self.add(Method::GET, path, &ops::READ, axum::routing::get(h))
    }

    fn post<H, T>(self, path: &'static str, op: &'static Op, h: H) -> Self
    where
        H: Handler<T, AppState>,
        T: 'static,
    {
        self.add(Method::POST, path, op, axum::routing::post(h))
    }

    fn put<H, T>(self, path: &'static str, op: &'static Op, h: H) -> Self
    where
        H: Handler<T, AppState>,
        T: 'static,
    {
        self.add(Method::PUT, path, op, axum::routing::put(h))
    }

    fn delete<H, T>(self, path: &'static str, op: &'static Op, h: H) -> Self
    where
        H: Handler<T, AppState>,
        T: 'static,
    {
        self.add(Method::DELETE, path, op, axum::routing::delete(h))
    }
}

/// Every route of the main API router with its classification. Built once.
pub fn route_table() -> &'static [RouteEntry] {
    static TABLE: OnceLock<Vec<RouteEntry>> = OnceLock::new();
    TABLE.get_or_init(|| governed_routes().table)
}

/// The single route list.
pub(crate) fn governed_routes() -> Routes {
    use crate::step_up::ceremony;
    use crate::*;
    Routes::new()
        .get("/healthz", healthz)
        .get("/readyz", readyz)
        .get("/metrics", metrics)
        .get("/openapi.json", openapi)
        .get("/.well-known/jwks.json", jwks)
        .get("/.well-known/openid-configuration", openid_configuration)
        .post("/v1/auth/login", &ops::AUTH_LOGIN, login)
        .get("/v1/auth/login", oauth_login_redirect)
        .get("/v1/auth/callback", oauth_callback)
        .get("/v1/auth/providers", auth_providers)
        .post("/v1/auth/logout", &ops::AUTH_LOGOUT, logout)
        .get("/v1/me", me)
        .post(
            "/v1/me/identities/link",
            &ops::IDENTITY_LINK,
            begin_link_identity,
        )
        .delete(
            "/v1/me/identities/{provider}",
            &ops::IDENTITY_UNLINK,
            unlink_identity,
        )
        // ADR-0073: the caller's passkeys and the step-up ceremony.
        .get("/v1/me/security", ceremony::me_security)
        .post(
            "/v1/me/passkeys/registration/options",
            &ops::PASSKEY_ADD,
            ceremony::passkey_registration_options,
        )
        .post(
            "/v1/me/passkeys/registration",
            &ops::PASSKEY_REGISTRATION,
            ceremony::passkey_registration,
        )
        .delete(
            "/v1/me/passkeys/{id}",
            &ops::PASSKEY_REMOVE,
            ceremony::remove_passkey,
        )
        .post(
            "/v1/me/step-up/options",
            &ops::STEP_UP_OPTIONS,
            ceremony::step_up_options,
        )
        .post(
            "/v1/me/step-up",
            &ops::STEP_UP_CONFIRM,
            ceremony::step_up_confirm,
        )
        // ADR-0073 §6-§8: the OrgSecurityEventLog, StepUpConfirmationRequests,
        // and an Owner's passkey reset. (The operator break-glass has no route.)
        .get(
            "/v1/me/security/events",
            step_up::events::my_security_events,
        )
        .get("/v1/security/events", step_up::events::security_events)
        .get(
            "/v1/orgs/{org}/security/events",
            step_up::events::org_security_events,
        )
        .get(
            "/v1/orgs/{org}/security/summary",
            step_up::events::org_security_summary,
        )
        .post(
            "/v1/orgs/{org}/members/{subject}/passkeys/reset",
            &ops::PASSKEY_RESET,
            step_up::admin::reset_member_passkeys,
        )
        .get("/v1/step-up/requests/{id}", step_up::requests::get_request)
        .post(
            "/v1/step-up/requests/{id}/options",
            &ops::STEP_UP_REQUEST_OPTIONS,
            step_up::requests::request_options,
        )
        .post(
            "/v1/step-up/requests/{id}/confirm",
            &ops::STEP_UP_REQUEST_CONFIRM,
            step_up::requests::confirm_request,
        )
        .get("/v1/orgs/{org}/bindings", list_bindings)
        .put("/v1/orgs/{org}/bindings", &ops::BINDING_PUT, put_binding)
        .delete(
            "/v1/orgs/{org}/bindings",
            &ops::BINDING_DELETE,
            delete_binding,
        )
        .get("/v1/orgs/{org}/principals", search_principals)
        .get("/v1/orgs/{org}/tokens", list_api_tokens)
        .post("/v1/orgs/{org}/tokens", &ops::TOKEN_MINT, mint_api_token)
        .delete(
            "/v1/orgs/{org}/tokens/{id}",
            &ops::TOKEN_REVOKE,
            revoke_api_token,
        )
        .post(
            "/v1/repos/{org}/{repo}/bindings/import",
            &ops::BINDINGS_IMPORT,
            import_bindings,
        )
        .get("/v1/repos", list_projects)
        .get("/v1/repos/{org}/{repo}/runs", list_repo_runs)
        .post("/v1/runs", &ops::RUN_CREATE, create_run)
        .get("/v1/runs", list_runs)
        .get("/v1/runs/{id}", get_run)
        .get("/v1/runs/{id}/events", get_events)
        .get("/v1/runs/{id}/logs", get_logs)
        .post(
            "/v1/runs/{id}/steps/{step}/rerun",
            &ops::STEP_RERUN,
            rerun_step,
        )
        // Deprecated alias for pre-rename callers (restart→rerun, 2026-07-23):
        // same handler, intentionally NOT in the OpenAPI surface.
        .post(
            "/v1/runs/{id}/steps/{step}/restart",
            &ops::STEP_RERUN,
            rerun_step,
        )
        .post(
            "/v1/runs/{id}/steps/{step}/retry",
            &ops::STEP_RETRY,
            retry_step,
        )
        // The rerun PREVIEW (ADR-0061 s5): what the rerun would actually execute,
        // read before the button is pressed so a widened scope is never a surprise.
        .get("/v1/runs/{id}/steps/{step}/rerun-plan", rerun_plan)
        // The manual Workspace-Snapshot pin (ADR-0061 s5) — one resource, two
        // verbs, because pin and unpin are the same fact in two states.
        .post(
            "/v1/runs/{id}/snapshots-pin",
            &ops::SNAPSHOTS_PIN,
            pin_run_snapshots,
        )
        .delete(
            "/v1/runs/{id}/snapshots-pin",
            &ops::SNAPSHOTS_UNPIN,
            unpin_run_snapshots,
        )
        .post("/v1/runs/{id}/cancel", &ops::RUN_CANCEL, cancel_run)
        .get("/v1/runs/{id}/artifacts", list_artifacts)
        .get("/v1/runs/{id}/artifacts/{*name}", download_artifact)
        .get("/v1/runs/{id}/steps/{step}/logs", get_step_logs)
        .get("/v1/runs/{id}/services", get_services)
        .get("/v1/runs/{id}/services/{service}/logs", get_service_logs)
        .get("/v1/runs/{id}/steps/{step}/attach", attach_step)
        .get("/v1/runs/{id}/steps/{step}/debug-pod", debug_pod_step)
        .get("/v1/runs/{id}/steps/{step}/workspace", list_workspace)
        .get(
            "/v1/runs/{id}/steps/{step}/workspace/file",
            get_workspace_file,
        )
        .post(
            "/v1/runs/{id}/gates/{step}/approve",
            &ops::GATE_APPROVE,
            approve_gate,
        )
        .post(
            "/v1/runs/{id}/gates/{step}/reject",
            &ops::GATE_REJECT,
            reject_gate,
        )
        .post(
            "/v1/runs/{id}/gates/{step}/release",
            &ops::GATE_RELEASE,
            release_gate_external,
        )
        .get("/v1/runs/{id}/steps/{step}/results", get_step_results)
        .post(
            "/v1/runs/{id}/steps/{step}/results",
            &ops::STEP_RESULTS,
            ingest_step_results,
        )
        .get("/v1/runs/{id}/steps/{step}/consumed", get_attempt_consumed)
        .post("/v1/secrets", &ops::SECRET_PUT, put_secret)
        .get("/v1/secrets", list_secrets)
        .delete("/v1/secrets", &ops::SECRET_DELETE, delete_secret)
        .get("/v1/repos/{org}/{repo}/environments", list_environments)
        .put(
            "/v1/repos/{org}/{repo}/environments/{name}",
            &ops::ENV_LEGACY_PUT,
            put_environment,
        )
        .get(
            "/v1/repos/{org}/{repo}/environments/{name}",
            get_environment,
        )
        .delete(
            "/v1/repos/{org}/{repo}/environments/{name}",
            &ops::ENV_LEGACY_DELETE,
            delete_environment,
        )
        .get(
            "/v1/repos/{org}/{repo}/environments/{name}/deployments",
            list_deployments,
        )
        .get("/v1/orgs/{org}/environments", oe::list_org_environments)
        .put(
            "/v1/orgs/{org}/environments/{env}",
            &ops::ENVIRONMENT_PUT,
            oe::put_org_environment,
        )
        .get("/v1/orgs/{org}/environments/{env}", oe::get_org_environment)
        .delete(
            "/v1/orgs/{org}/environments/{env}",
            &ops::ENVIRONMENT_DELETE,
            oe::delete_org_environment,
        )
        .put(
            "/v1/orgs/{org}/environments/{env}/eligibility",
            &ops::ELIGIBILITY_PUT,
            oe::put_environment_eligibility,
        )
        .get(
            "/v1/orgs/{org}/environments/{env}/eligibility",
            oe::get_environment_eligibility,
        )
        .put(
            "/v1/orgs/{org}/environments/{env}/protection",
            &ops::PROTECTION_PUT,
            oe::put_environment_protection,
        )
        .get(
            "/v1/orgs/{org}/environments/{env}/protection",
            oe::get_environment_protection,
        )
        .get(
            "/v1/orgs/{org}/environments/{env}/projects",
            oe::list_environment_projects,
        )
        .put(
            "/v1/orgs/{org}/environments/{env}/projects/{project}/overlay",
            &ops::OVERLAY_PUT,
            oe::put_environment_project_overlay,
        )
        .get(
            "/v1/orgs/{org}/environments/{env}/projects/{project}/overlay",
            oe::get_environment_project_overlay,
        )
        .delete(
            "/v1/orgs/{org}/environments/{env}/projects/{project}/overlay",
            &ops::OVERLAY_DELETE,
            oe::delete_environment_project_overlay,
        )
        .put(
            "/v1/orgs/{org}/environments/{env}/projects/{project}/privilege-grant",
            &ops::PRIVILEGE_GRANT_PUT,
            oe::put_environment_project_privilege_grant,
        )
        .get(
            "/v1/orgs/{org}/environments/{env}/projects/{project}/privilege-grant",
            oe::get_environment_project_privilege_grant,
        )
        .delete(
            "/v1/orgs/{org}/environments/{env}/projects/{project}/privilege-grant",
            &ops::PRIVILEGE_GRANT_DELETE,
            oe::delete_environment_project_privilege_grant,
        )
        .get(
            "/v1/orgs/{org}/environment-migration-report",
            oe::get_environment_migration_report,
        )
        .post(
            "/v1/orgs/{org}/environment-migration-report/{env}/acknowledge",
            &ops::MIGRATION_ACK,
            oe::acknowledge_environment_migration_report,
        )
        .get(
            "/v1/orgs/{org}/environments/{env}/deployment-targets",
            plugins::environment_deployment_targets,
        )
        .get("/v1/orgs/{org}/plugin-catalog", plugins::plugin_catalog)
        .get("/v1/orgs/{org}/plugins", plugins::list_plugins)
        .post(
            "/v1/orgs/{org}/plugins",
            &ops::PLUGIN_INSTALL,
            plugins::install_plugin,
        )
        .get("/v1/orgs/{org}/plugins/{name}", plugins::get_plugin)
        .delete(
            "/v1/orgs/{org}/plugins/{name}",
            &ops::PLUGIN_UNINSTALL,
            plugins::uninstall_plugin,
        )
        .post(
            "/v1/orgs/{org}/plugins/{name}/kill",
            &ops::PLUGIN_KILL,
            plugins::kill_plugin,
        )
        .post(
            "/v1/orgs/{org}/plugins/{name}/enable",
            &ops::PLUGIN_ENABLE,
            plugins::enable_plugin,
        )
        .get(
            "/v1/orgs/{org}/plugins/{name}/unmapped-groupings",
            plugins::unmapped_groupings,
        )
        .get(
            "/v1/orgs/{org}/plugins/{name}/connections",
            plugins::list_plugin_connections,
        )
        .put(
            "/v1/orgs/{org}/plugins/{name}/connections/{slot}",
            &ops::PLUGIN_CONNECTION_PUT,
            plugins::put_plugin_connection,
        )
        .delete(
            "/v1/orgs/{org}/plugins/{name}/connections/{slot}",
            &ops::PLUGIN_CONNECTION_DELETE,
            plugins::delete_plugin_connection,
        )
        .get("/v1/orgs/{org}/board", get_environment_board)
        .get(
            "/v1/orgs/{org}/environments/{env}/deployments",
            list_environment_deployments,
        )
        .get(
            "/v1/orgs/{org}/environments/{env}/deployments/{id}",
            get_deployment,
        )
        .get("/v1/connections", list_connections)
        .post(
            "/v1/connections",
            &ops::CONNECTION_CREATE,
            create_connection,
        )
        .delete(
            "/v1/connections/{id}",
            &ops::CONNECTION_DELETE,
            delete_connection,
        )
        .post(
            "/v1/connections/{id}/resync",
            &ops::CONNECTION_RESYNC,
            resync_connection,
        )
        .get("/v1/connections/{id}/preflight", connection_preflight)
        .get("/v1/connections/{id}/available-repos", available_repos)
        .post(
            "/v1/connections/{id}/repos",
            &ops::CONNECTION_BIND_REPO,
            bind_repo,
        )
        .delete(
            "/v1/connections/{id}/repos/{owner}/{name}",
            &ops::CONNECTION_UNBIND_REPO,
            unbind_repo,
        )
        .post(
            "/v1/connections/{id}/repos/{owner}/{name}/webhook",
            &ops::CONNECTION_WEBHOOK,
            register_repo_webhook,
        )
        .get("/v1/repos/{org}/{repo}/secrets/matrix", secret_matrix)
        .put(
            "/v1/repos/{org}/{repo}/secrets/matrix/silenced",
            &ops::SECRET_CELL_SILENCE,
            silence_secret_cell,
        )
        .delete(
            "/v1/repos/{org}/{repo}/secrets/matrix/silenced",
            &ops::SECRET_CELL_UNSILENCE,
            unsilence_secret_cell,
        )
        .get("/v1/repos/{org}/{repo}/refs", list_refs)
        .get("/v1/repos/{org}/{repo}/pipelines", list_pipelines)
        .get(
            "/v1/repos/{org}/{repo}/pipelines/{name}/interface",
            pipeline_interface,
        )
        .post("/v1/repos/{org}/{repo}/dispatch", &ops::DISPATCH, dispatch)
        .post("/webhooks/github", &ops::WEBHOOK_GITHUB, github_webhook)
        .post("/webhooks/forgejo", &ops::WEBHOOK_FORGEJO, forgejo_webhook)
}
