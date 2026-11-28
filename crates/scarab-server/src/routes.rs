//! The one route list of the main API router (ADR-0073 §3; "The guard" in
//! its Implementation notes).
//!
//! Every route is registered through [`Routes`], which takes the route's
//! [`Op`] classification as an argument: no route, a GET included, can be
//! registered without naming its `&'static Op` (ADR-0086 §5: a GET names
//! the read operation of the data it exposes). So a new mutating route
//! cannot exist without saying whether it is a crown jewel, a new read
//! cannot exist without saying what it exposes, and [`route_table`] is the
//! complete, test-enumerable list the step-up guard consults at runtime. A
//! mutating request the table does not know is refused 500 by the guard
//! (fail-closed).
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
use crate::{libraries as lib, org_environments as oe, policies as pol, AppState};

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

    /// A GET names its operation like any other route: a read operation
    /// named by the data it exposes ([`ops::RUN_READ`], …), or, for the
    /// debug shell's WebSocket upgrade, a crown jewel the guard governs like
    /// a write.
    fn get<H, T>(self, path: &'static str, op: &'static Op, h: H) -> Self
    where
        H: Handler<T, AppState>,
        T: 'static,
    {
        self.add(Method::GET, path, op, axum::routing::get(h))
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
        .get("/healthz", &ops::SERVER_READ, healthz)
        .get("/livez", &ops::SERVER_READ, livez)
        .get("/readyz", &ops::SERVER_READ, readyz)
        .get("/metrics", &ops::SERVER_READ, metrics)
        .get("/openapi.json", &ops::SERVER_READ, openapi)
        .get("/.well-known/jwks.json", &ops::SERVER_READ, jwks)
        .get(
            "/.well-known/openid-configuration",
            &ops::SERVER_READ,
            openid_configuration,
        )
        .post("/v1/auth/login", &ops::AUTH_LOGIN, login)
        .get("/v1/auth/login", &ops::SERVER_READ, oauth_login_redirect)
        .get("/v1/auth/callback", &ops::SERVER_READ, oauth_callback)
        .get("/v1/auth/providers", &ops::SERVER_READ, auth_providers)
        .post("/v1/auth/logout", &ops::AUTH_LOGOUT, logout)
        .get("/v1/me", &ops::ACCOUNT_READ, me)
        .get("/v1/about", &ops::SERVER_READ, about)
        .post(
            "/v1/me/identities/link",
            &ops::IDENTITY_LINK,
            begin_link_identity,
        )
        .get(
            "/v1/me/identities/merges/{id}",
            &ops::ACCOUNT_READ,
            identity_merge,
        )
        .post(
            "/v1/me/identities/merges/{id}",
            &ops::IDENTITY_MERGE_CONFIRM,
            confirm_identity_merge,
        )
        .delete(
            "/v1/me/identities/{provider}",
            &ops::IDENTITY_UNLINK,
            unlink_identity,
        )
        // ADR-0073: the caller's passkeys and the step-up ceremony.
        .get("/v1/me/security", &ops::ACCOUNT_READ, ceremony::me_security)
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
            &ops::ACCOUNT_READ,
            step_up::events::my_security_events,
        )
        // The caller's own API tokens (git-bug a4cb94f).
        .get("/v1/me/tokens", &ops::ACCOUNT_READ, list_own_api_tokens)
        .post("/v1/me/tokens", &ops::TOKEN_MINT_OWN, mint_own_api_token)
        .delete(
            "/v1/me/tokens/{id}",
            &ops::TOKEN_REVOKE_OWN,
            revoke_own_api_token,
        )
        .get(
            "/v1/security/events",
            &ops::SECURITY_READ,
            step_up::events::security_events,
        )
        .get(
            "/v1/orgs/{org}/security/events",
            &ops::SECURITY_READ,
            step_up::events::org_security_events,
        )
        .get(
            "/v1/orgs/{org}/security/summary",
            &ops::SECURITY_READ,
            step_up::events::org_security_summary,
        )
        // An org's leak check limits: Admins read them, Owners change them.
        .get(
            "/v1/orgs/{org}/security/leak-checks",
            &ops::SECURITY_READ,
            crate::secret_match::get_leak_check_limits,
        )
        .put(
            "/v1/orgs/{org}/security/leak-checks",
            &ops::LEAK_CHECK_LIMITS_PUT,
            crate::secret_match::put_leak_check_limits,
        )
        // An org's Fork Run approval setting (ADR-0086 §9): members read
        // it, admins change it from a browser.
        .get(
            "/v1/orgs/{org}/security/fork-run-approval",
            &ops::SETTINGS_READ,
            crate::fork_run_approval::get_fork_run_approval,
        )
        .put(
            "/v1/orgs/{org}/security/fork-run-approval",
            &ops::FORK_RUN_APPROVAL_PUT,
            crate::fork_run_approval::put_fork_run_approval,
        )
        .post(
            "/v1/orgs/{org}/members/{id}/passkeys/reset",
            &ops::PASSKEY_RESET,
            step_up::admin::reset_member_passkeys,
        )
        .get(
            "/v1/step-up/requests/{id}",
            &ops::ACCOUNT_READ,
            step_up::requests::get_request,
        )
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
        .get("/v1/orgs/{org}/bindings", &ops::ACCESS_READ, list_bindings)
        .put("/v1/orgs/{org}/bindings", &ops::BINDING_PUT, put_binding)
        .delete(
            "/v1/orgs/{org}/bindings",
            &ops::BINDING_DELETE,
            delete_binding,
        )
        .get(
            "/v1/orgs/{org}/principals",
            &ops::ACCESS_READ,
            search_principals,
        )
        .get("/v1/orgs/{org}/members/{id}", &ops::ACCESS_READ, get_member)
        .get("/v1/orgs/{org}/tokens", &ops::ACCESS_READ, list_api_tokens)
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
        .get("/v1/repos", &ops::REPO_READ, list_projects)
        .get(
            "/v1/repos/{org}/{repo}/runs",
            &ops::RUN_READ,
            list_repo_runs,
        )
        .get(
            "/v1/repos/{org}/{repo}/refused-runs",
            &ops::RUN_READ,
            refused_runs::list_refused_runs,
        )
        .get(
            "/v1/repos/{org}/{repo}/schedules",
            &ops::REPO_READ,
            crate::schedules::list_schedules,
        )
        .post("/v1/runs", &ops::RUN_CREATE, create_run)
        .get("/v1/runs", &ops::RUN_READ, list_runs)
        .get("/v1/runs/{id}", &ops::RUN_READ, get_run)
        .get("/v1/runs/{id}/events", &ops::RUN_READ, get_events)
        .get("/v1/runs/{id}/logs", &ops::STEP_LOGS_READ, get_logs)
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
        .get(
            "/v1/runs/{id}/steps/{step}/rerun-plan",
            &ops::RUN_READ,
            rerun_plan,
        )
        .get(
            "/v1/runs/{id}/steps/{step}/origin",
            &ops::RUN_READ,
            run_origin::get_step_origin,
        )
        .get(
            "/v1/runs/{id}/manifest",
            &ops::RUN_READ,
            run_origin::get_run_manifest,
        )
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
        .get(
            "/v1/runs/{id}/artifacts",
            &ops::RUN_ARTIFACTS_READ,
            list_artifacts,
        )
        .get(
            "/v1/runs/{id}/artifacts/{*name}",
            &ops::RUN_ARTIFACTS_READ,
            download_artifact,
        )
        .get(
            "/v1/runs/{id}/steps/{step}/logs",
            &ops::STEP_LOGS_READ,
            get_step_logs,
        )
        .get(
            "/v1/runs/{id}/steps/{step}/logs/raw",
            &ops::STEP_LOGS_READ,
            get_step_logs_raw,
        )
        .get("/v1/runs/{id}/services", &ops::RUN_READ, get_services)
        .get(
            "/v1/runs/{id}/services/{service}/logs",
            &ops::STEP_LOGS_READ,
            get_service_logs,
        )
        .get(
            "/v1/runs/{id}/services/{service}/logs/raw",
            &ops::STEP_LOGS_READ,
            get_service_logs_raw,
        )
        .get(
            "/v1/runs/{id}/steps/{step}/attach",
            &ops::STEP_ATTACH,
            attach_step,
        )
        .get(
            "/v1/runs/{id}/steps/{step}/debug-pod",
            &ops::STEP_DEBUG_POD,
            debug_pod_step,
        )
        .get(
            "/v1/runs/{id}/steps/{step}/workspace",
            &ops::STEP_WORKSPACE_READ,
            list_workspace,
        )
        .get(
            "/v1/runs/{id}/steps/{step}/workspace/file",
            &ops::STEP_WORKSPACE_READ,
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
        .get(
            "/v1/runs/{id}/steps/{step}/results",
            &ops::RUN_READ,
            get_step_results,
        )
        .post(
            "/v1/runs/{id}/steps/{step}/results",
            &ops::STEP_RESULTS,
            ingest_step_results,
        )
        .get(
            "/v1/runs/{id}/steps/{step}/consumed",
            &ops::RUN_READ,
            get_attempt_consumed,
        )
        .post("/v1/secrets", &ops::SECRET_PUT, put_secret)
        .get("/v1/secrets", &ops::SECRET_NAMES_READ, list_secrets)
        .delete("/v1/secrets", &ops::SECRET_DELETE, delete_secret)
        .get(
            "/v1/repos/{org}/{repo}/environments",
            &ops::SETTINGS_READ,
            list_environments,
        )
        .put(
            "/v1/repos/{org}/{repo}/environments/{name}",
            &ops::ENV_LEGACY_PUT,
            put_environment,
        )
        .get(
            "/v1/repos/{org}/{repo}/environments/{name}",
            &ops::SETTINGS_READ,
            get_environment,
        )
        .delete(
            "/v1/repos/{org}/{repo}/environments/{name}",
            &ops::ENV_LEGACY_DELETE,
            delete_environment,
        )
        .get(
            "/v1/orgs/{org}/environments",
            &ops::SETTINGS_READ,
            oe::list_org_environments,
        )
        .get(
            "/v1/orgs/{org}/secrets/orphaned",
            &ops::SECRET_NAMES_READ,
            oe::list_orphaned_secrets,
        )
        .get(
            "/v1/orgs/{org}/secrets/matrix",
            &ops::SECRET_NAMES_READ,
            org_secret_matrix,
        )
        .post(
            "/v1/orgs/{org}/secrets/match",
            &ops::SECRET_MATCH,
            crate::secret_match::org_secret_match,
        )
        .put(
            "/v1/orgs/{org}/environments/{env}",
            &ops::ENVIRONMENT_PUT,
            oe::put_org_environment,
        )
        .get(
            "/v1/orgs/{org}/environments/{env}",
            &ops::SETTINGS_READ,
            oe::get_org_environment,
        )
        .delete(
            "/v1/orgs/{org}/environments/{env}",
            &ops::ENVIRONMENT_DELETE,
            oe::delete_org_environment,
        )
        .post(
            "/v1/orgs/{org}/environments/{env}/duplicate",
            &ops::ENVIRONMENT_DUPLICATE,
            oe::duplicate_org_environment,
        )
        .put(
            "/v1/orgs/{org}/environments/{env}/eligibility",
            &ops::ELIGIBILITY_PUT,
            oe::put_environment_eligibility,
        )
        .get(
            "/v1/orgs/{org}/environments/{env}/eligibility",
            &ops::SETTINGS_READ,
            oe::get_environment_eligibility,
        )
        .put(
            "/v1/orgs/{org}/environments/{env}/protection",
            &ops::PROTECTION_PUT,
            oe::put_environment_protection,
        )
        .get(
            "/v1/orgs/{org}/environments/{env}/protection",
            &ops::SETTINGS_READ,
            oe::get_environment_protection,
        )
        .get(
            "/v1/orgs/{org}/environments/{env}/projects",
            &ops::SETTINGS_READ,
            oe::list_environment_projects,
        )
        .put(
            "/v1/orgs/{org}/environments/{env}/projects/{project}/overlay",
            &ops::OVERLAY_PUT,
            oe::put_environment_project_overlay,
        )
        .get(
            "/v1/orgs/{org}/environments/{env}/projects/{project}/overlay",
            &ops::SETTINGS_READ,
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
            &ops::SETTINGS_READ,
            oe::get_environment_project_privilege_grant,
        )
        .delete(
            "/v1/orgs/{org}/environments/{env}/projects/{project}/privilege-grant",
            &ops::PRIVILEGE_GRANT_DELETE,
            oe::delete_environment_project_privilege_grant,
        )
        // ADR-0075: org Policies, Project typed settings, decisions, the test API.
        .get(
            "/v1/orgs/{org}/policies",
            &ops::SETTINGS_READ,
            pol::list_org_policies,
        )
        .post(
            "/v1/orgs/{org}/policies",
            &ops::POLICY_CREATE,
            pol::create_org_policy,
        )
        .get(
            "/v1/orgs/{org}/policies/{id}",
            &ops::SETTINGS_READ,
            pol::get_org_policy,
        )
        .put(
            "/v1/orgs/{org}/policies/{id}",
            &ops::POLICY_UPDATE,
            pol::update_org_policy,
        )
        .delete(
            "/v1/orgs/{org}/policies/{id}",
            &ops::POLICY_DELETE,
            pol::delete_org_policy,
        )
        .get(
            "/v1/orgs/{org}/policies/{id}/versions",
            &ops::SETTINGS_READ,
            pol::list_org_policy_versions,
        )
        .get(
            "/v1/orgs/{org}/projects/{project}/policies",
            &ops::SETTINGS_READ,
            pol::list_project_policies,
        )
        .post(
            "/v1/orgs/{org}/projects/{project}/policies",
            &ops::PROJECT_POLICY_CREATE,
            pol::create_project_policy,
        )
        .get(
            "/v1/orgs/{org}/projects/{project}/policies/{id}",
            &ops::SETTINGS_READ,
            pol::get_project_policy,
        )
        .put(
            "/v1/orgs/{org}/projects/{project}/policies/{id}",
            &ops::PROJECT_POLICY_UPDATE,
            pol::update_project_policy,
        )
        .delete(
            "/v1/orgs/{org}/projects/{project}/policies/{id}",
            &ops::PROJECT_POLICY_DELETE,
            pol::delete_project_policy,
        )
        .get(
            "/v1/orgs/{org}/projects/{project}/policies/{id}/versions",
            &ops::SETTINGS_READ,
            pol::list_project_policy_versions,
        )
        .post(
            "/v1/orgs/{org}/policies/check",
            &ops::POLICY_CHECK,
            pol::check_org_policy,
        )
        .post(
            "/v1/orgs/{org}/policy-test",
            &ops::POLICY_TEST,
            pol::policy_test,
        )
        .post(
            "/v1/orgs/{org}/policy-test/replay",
            &ops::POLICY_REPLAY,
            pol::policy_test_replay,
        )
        .get(
            "/v1/orgs/{org}/policy-decisions",
            &ops::SETTINGS_READ,
            pol::list_org_policy_decisions,
        )
        .get(
            "/v1/orgs/{org}/policy-decisions/counts",
            &ops::SETTINGS_READ,
            pol::org_policy_decision_counts,
        )
        .get(
            "/v1/policy-checkpoints",
            &ops::SETTINGS_READ,
            pol::list_policy_checkpoints,
        )
        .get(
            "/v1/runs/{id}/policy-decisions",
            &ops::RUN_READ,
            pol::list_run_policy_decisions,
        )
        // ADR-0076: org-installed libraries, yanks, trusted sources.
        .get(
            "/v1/orgs/{org}/libraries",
            &ops::SETTINGS_READ,
            lib::list_libraries,
        )
        .get(
            "/v1/orgs/{org}/library-installations/{id}",
            &ops::SETTINGS_READ,
            lib::get_library_installation,
        )
        .get(
            "/v1/orgs/{org}/library-yanks/{hash}/impact",
            &ops::SETTINGS_READ,
            lib::get_yank_impact,
        )
        .post(
            "/v1/orgs/{org}/libraries/fetch",
            &ops::LIBRARY_FETCH,
            lib::fetch_library,
        )
        .post(
            "/v1/orgs/{org}/library-installations",
            &ops::LIBRARY_INSTALL,
            lib::install_library,
        )
        .delete(
            "/v1/orgs/{org}/library-installations/{id}",
            &ops::LIBRARY_REMOVE,
            lib::remove_library,
        )
        .post(
            "/v1/orgs/{org}/library-yanks",
            &ops::LIBRARY_YANK,
            lib::yank_library,
        )
        .delete(
            "/v1/orgs/{org}/library-yanks/{hash}",
            &ops::LIBRARY_UNYANK,
            lib::unyank_library,
        )
        .post(
            "/v1/orgs/{org}/library-trust",
            &ops::LIBRARY_TRUST,
            lib::trust_library_source,
        )
        .delete(
            "/v1/orgs/{org}/library-trust",
            &ops::LIBRARY_UNTRUST,
            lib::untrust_library_source,
        )
        .post(
            "/v1/orgs/{org}/plugin-fetch",
            &ops::PLUGIN_FETCH,
            plugins::fetch_plugin_manifest,
        )
        .get(
            "/v1/orgs/{org}/plugins",
            &ops::SETTINGS_READ,
            plugins::list_plugins,
        )
        .get(
            "/v1/orgs/{org}/gates",
            &ops::RUN_READ,
            crate::plugin_gates::list_pending_gates,
        )
        .post(
            "/v1/orgs/{org}/plugins",
            &ops::PLUGIN_INSTALL,
            plugins::install_plugin,
        )
        .get(
            "/v1/orgs/{org}/plugins/{name}",
            &ops::SETTINGS_READ,
            plugins::get_plugin,
        )
        .get(
            "/v1/orgs/{org}/plugin-installations/{id}",
            &ops::SETTINGS_READ,
            plugins::get_plugin_by_id,
        )
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
        .post(
            "/v1/orgs/{org}/plugins/{name}/upgrade-fetch",
            &ops::PLUGIN_UPGRADE_FETCH,
            plugins::fetch_plugin_upgrade,
        )
        .post(
            "/v1/orgs/{org}/plugins/{name}/upgrade",
            &ops::PLUGIN_UPGRADE,
            plugins::upgrade_plugin,
        )
        .put(
            "/v1/orgs/{org}/plugins/{name}/storage",
            &ops::PLUGIN_STORAGE_PUT,
            plugins::put_plugin_storage,
        )
        .delete(
            "/v1/orgs/{org}/plugins/{name}/storage",
            &ops::PLUGIN_STORAGE_DELETE,
            plugins::delete_plugin_storage,
        )
        .get(
            "/v1/orgs/{org}/plugins/{name}/connections",
            &ops::SETTINGS_READ,
            plugins::list_plugin_connections,
        )
        .get(
            "/v1/orgs/{org}/plugins/{name}/broker-journal",
            &ops::SECURITY_READ,
            plugins::plugin_broker_journal,
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
        .get("/v1/connections", &ops::SETTINGS_READ, list_connections)
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
        .put(
            "/v1/connections/{id}/org",
            &ops::CONNECTION_ASSIGN_ORG,
            assign_connection_org,
        )
        .post(
            "/v1/connections/{id}/resync",
            &ops::CONNECTION_RESYNC,
            resync_connection,
        )
        .get(
            "/v1/connections/{id}/preflight",
            &ops::SETTINGS_READ,
            connection_preflight,
        )
        .get(
            "/v1/forges/github/app",
            &ops::SETTINGS_READ,
            github_app_info,
        )
        .get(
            "/v1/connections/{id}/available-repos",
            &ops::SETTINGS_READ,
            available_repos,
        )
        .get("/v1/orgs/{org}/repos", &ops::REPO_READ, org_repos)
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
        .get(
            "/v1/repos/{org}/{repo}/secrets/matrix",
            &ops::SECRET_NAMES_READ,
            secret_matrix,
        )
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
        .get("/v1/repos/{org}/{repo}/refs", &ops::REPO_READ, list_refs)
        .get(
            "/v1/repos/{org}/{repo}/pipelines",
            &ops::REPO_READ,
            list_pipelines,
        )
        .get(
            "/v1/repos/{org}/{repo}/pipelines/{name}/interface",
            &ops::REPO_READ,
            pipeline_interface,
        )
        .post("/v1/repos/{org}/{repo}/dispatch", &ops::DISPATCH, dispatch)
        .post("/webhooks/github", &ops::WEBHOOK_GITHUB, github_webhook)
        .post("/webhooks/forgejo", &ops::WEBHOOK_FORGEJO, forgejo_webhook)
}
