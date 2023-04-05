// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// BEGIN LINT CONFIG
// DO NOT EDIT. Automatically generated by bin/gen-lints.
// Have complaints about the noise? See the note in misc/python/materialize/cli/gen-lints.py first.
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(clippy::double_neg)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::collapsible_if)]
#![warn(clippy::collapsible_else_if)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::overflow_check_conditional)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::disallowed_types)]
#![warn(clippy::from_over_into)]
// END LINT CONFIG

//! Manages a single Materialize environment.
//!
//! It listens for SQL connections on port 6875 (MTRL) and for HTTP connections
//! on port 6876.

use std::cmp;
use std::env;
use std::ffi::CStr;
use std::iter;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::{bail, Context};
use clap::{ArgEnum, Parser};
use fail::FailScenario;
use http::header::HeaderValue;
use itertools::Itertools;
use jsonwebtoken::DecodingKey;
use once_cell::sync::Lazy;
use opentelemetry::trace::TraceContextExt;
use prometheus::IntGauge;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use url::Url;
use uuid::Uuid;

use mz_adapter::catalog::ClusterReplicaSizeMap;
use mz_cloud_resources::{AwsExternalIdPrefix, CloudResourceController};
use mz_controller::ControllerConfig;
use mz_environmentd::{TlsConfig, TlsMode, BUILD_INFO};
use mz_frontegg_auth::{
    Authentication as FronteggAuthentication, AuthenticationConfig as FronteggConfig,
};
use mz_orchestrator::Orchestrator;
use mz_orchestrator_kubernetes::{
    KubernetesImagePullPolicy, KubernetesOrchestrator, KubernetesOrchestratorConfig,
};
use mz_orchestrator_process::{
    ProcessOrchestrator, ProcessOrchestratorConfig, ProcessOrchestratorTcpProxyConfig,
};
use mz_orchestrator_tracing::{StaticTracingConfig, TracingCliArgs, TracingOrchestrator};
use mz_ore::cli::{self, CliConfig, KeyValueArg};
use mz_ore::metric;
use mz_ore::metrics::MetricsRegistry;
use mz_ore::now::SYSTEM_TIME;
use mz_persist_client::cache::PersistClientCache;
use mz_persist_client::cfg::PersistConfig;
use mz_persist_client::PersistLocation;
use mz_secrets::SecretsController;
use mz_service::emit_boot_diagnostics;
use mz_sql::catalog::EnvironmentId;
use mz_stash::StashFactory;
use mz_storage_client::types::connections::ConnectionContext;

mod sys;

static VERSION: Lazy<String> = Lazy::new(|| BUILD_INFO.human_version());
static LONG_VERSION: Lazy<String> = Lazy::new(|| {
    iter::once(BUILD_INFO.human_version())
        .chain(build_info())
        .join("\n")
});

/// Manages a single Materialize environment.
#[derive(Parser, Debug)]
#[clap(
    name = "environmentd",
    next_line_help = true,
    version = VERSION.as_str(),
    long_version = LONG_VERSION.as_str(),
)]
pub struct Args {
    // === Special modes. ===
    /// Enable unsafe features.
    ///
    /// Unsafe features fall into two categories:
    ///
    ///   * In-development features that are not yet ready for production use.
    ///   * Features useful for development and testing that would pose a
    ///     legitimate security risk if used in Materialize Cloud.
    #[clap(long, env = "UNSAFE_MODE")]
    unsafe_mode: bool,

    // === Connection options. ===
    /// The address on which to listen for untrusted SQL connections.
    ///
    /// Connections on this address are subject to encryption, authentication,
    /// and authorization as specified by the `--tls-mode` and `--frontegg-auth`
    /// options.
    #[clap(
        long,
        env = "SQL_LISTEN_ADDR",
        value_name = "HOST:PORT",
        default_value = "127.0.0.1:6875"
    )]
    sql_listen_addr: SocketAddr,
    /// The address on which to listen for untrusted HTTP connections.
    ///
    /// Connections on this address are subject to encryption, authentication,
    /// and authorization as specified by the `--tls-mode` and `--frontegg-auth`
    /// options.
    #[clap(
        long,
        env = "HTTP_LISTEN_ADDR",
        value_name = "HOST:PORT",
        default_value = "127.0.0.1:6876"
    )]
    http_listen_addr: SocketAddr,
    /// The address on which to listen for trusted SQL connections.
    ///
    /// Connections to this address are not subject to encryption, authentication,
    /// or access control. Care should be taken to not expose this address to the
    /// public internet
    /// or other unauthorized parties.
    #[clap(
        long,
        value_name = "HOST:PORT",
        env = "INTERNAL_SQL_LISTEN_ADDR",
        default_value = "127.0.0.1:6877"
    )]
    internal_sql_listen_addr: SocketAddr,
    /// The address on which to listen for trusted HTTP connections.
    ///
    /// Connections to this address are not subject to encryption, authentication,
    /// or access control. Care should be taken to not expose the listen address
    /// to the public internet or other unauthorized parties.
    #[clap(
        long,
        value_name = "HOST:PORT",
        env = "INTERNAL_HTTP_LISTEN_ADDR",
        default_value = "127.0.0.1:6878"
    )]
    internal_http_listen_addr: SocketAddr,
    /// Enable cross-origin resource sharing (CORS) for HTTP requests from the
    /// specified origin.
    ///
    /// The default allows all local connections.
    /// "*" allows all.
    /// "*.domain.com" allows connections from any matching subdomain.
    ///
    /// Wildcards in other positions (e.g., "https://*.foo.com" or "https://foo.*.com") have no effect.
    #[structopt(long, env = "CORS_ALLOWED_ORIGIN")]
    cors_allowed_origin: Vec<HeaderValue>,
    /// How stringently to demand TLS authentication and encryption.
    ///
    /// If set to "disable", then environmentd rejects HTTP and PostgreSQL
    /// connections that negotiate TLS.
    ///
    /// If set to "require", then environmentd requires that all HTTP and
    /// PostgreSQL connections negotiate TLS. Unencrypted connections will be
    /// rejected.
    #[clap(
        long, env = "TLS_MODE",
        possible_values = &["disable", "require"],
        default_value = "disable",
        default_value_ifs = &[
            ("frontegg-tenant", None, Some("require")),
        ],
        value_name = "MODE",
    )]
    tls_mode: String,
    /// Certificate file for TLS connections.
    #[clap(
        long,
        env = "TLS_CERT",
        requires = "tls-key",
        required_if_eq_any(&[("tls-mode", "require")]),
        value_name = "PATH"
    )]
    tls_cert: Option<PathBuf>,
    /// Private key file for TLS connections.
    #[clap(
        long,
        env = "TLS_KEY",
        requires = "tls-cert",
        required_if_eq_any(&[("tls-mode", "require")]),
        value_name = "PATH"
    )]
    tls_key: Option<PathBuf>,
    /// Enables Frontegg authentication for the specified tenant ID.
    #[clap(
        long,
        env = "FRONTEGG_TENANT",
        requires_all = &["frontegg-jwk", "frontegg-api-token-url", "frontegg-admin-role"],
        value_name = "UUID",
    )]
    frontegg_tenant: Option<Uuid>,
    /// JWK used to validate JWTs during Frontegg authentication as a PEM public
    /// key. Can optionally be base64 encoded with the URL-safe alphabet.
    #[clap(long, env = "FRONTEGG_JWK", requires = "frontegg-tenant")]
    frontegg_jwk: Option<String>,
    /// The full URL (including path) to the Frontegg api-token endpoint.
    #[clap(long, env = "FRONTEGG_API_TOKEN_URL", requires = "frontegg-tenant")]
    frontegg_api_token_url: Option<String>,
    /// The name of the admin role in Frontegg.
    #[clap(long, env = "FRONTEGG_ADMIN_ROLE", requires = "frontegg-tenant")]
    frontegg_admin_role: Option<String>,

    // === Orchestrator options. ===
    /// The service orchestrator implementation to use.
    #[structopt(long, arg_enum, env = "ORCHESTRATOR")]
    orchestrator: OrchestratorKind,
    /// Name of a non-default Kubernetes scheduler, if any.
    #[structopt(long, env = "ORCHESTRATOR_KUBERNETES_SCHEDULER_NAME")]
    orchestrator_kubernetes_scheduler_name: Option<String>,
    /// Labels to apply to all services created by the Kubernetes orchestrator
    /// in the form `KEY=VALUE`.
    #[structopt(long, env = "ORCHESTRATOR_KUBERNETES_SERVICE_LABEL")]
    orchestrator_kubernetes_service_label: Vec<KeyValueArg<String, String>>,
    /// Node selector to apply to all services created by the Kubernetes
    /// orchestrator in the form `KEY=VALUE`.
    #[structopt(long, env = "ORCHESTRATOR_KUBERNETES_SERVICE_NODE_SELECTOR")]
    orchestrator_kubernetes_service_node_selector: Vec<KeyValueArg<String, String>>,
    /// The name of a service account to apply to all services created by the
    /// Kubernetes orchestrator.
    #[structopt(long, env = "ORCHESTRATOR_KUBERNETES_SERVICE_ACCOUNT")]
    orchestrator_kubernetes_service_account: Option<String>,
    /// The Kubernetes context to use with the Kubernetes orchestrator.
    ///
    /// This defaults to `minikube` to prevent disaster (e.g., connecting to a
    /// production cluster that happens to be the active Kubernetes context.)
    #[structopt(
        long,
        env = "ORCHESTRATOR_KUBERNETES_CONTEXT",
        default_value = "minikube"
    )]
    orchestrator_kubernetes_context: String,
    /// The image pull policy to use for services created by the Kubernetes
    /// orchestrator.
    #[structopt(
        long,
        env = "ORCHESTRATOR_KUBERNETES_IMAGE_PULL_POLICY",
        default_value = "always",
        arg_enum
    )]
    orchestrator_kubernetes_image_pull_policy: KubernetesImagePullPolicy,
    /// The init container for services created by the Kubernetes orchestrator.
    #[clap(long, env = "ORCHESTRATOR_KUBERNETES_INIT_CONTAINER_IMAGE")]
    orchestrator_kubernetes_init_container_image: Option<String>,
    /// Prefix commands issued by the process orchestrator with the supplied
    /// value.
    #[clap(long, env = "ORCHESTRATOR_PROCESS_WRAPPER")]
    orchestrator_process_wrapper: Option<String>,
    /// Where the process orchestrator should store secrets.
    #[clap(
        long,
        env = "ORCHESTRATOR_PROCESS_SECRETS_DIRECTORY",
        value_name = "PATH",
        required_if_eq("orchestrator", "process")
    )]
    orchestrator_process_secrets_directory: Option<PathBuf>,
    /// Whether the process orchestrator should handle crashes in child
    /// processes by crashing the parent process.
    #[clap(long, env = "ORCHESTRATOR_PROCESS_PROPAGATE_CRASHES")]
    orchestrator_process_propagate_crashes: bool,
    /// An IP address on which the process orchestrator should bind TCP proxies
    /// for Unix domain sockets.
    ///
    /// When specified, for each named port of each created service, the process
    /// orchestrator will bind a TCP listener to the specified address that
    /// proxies incoming connections to the underlying Unix domain socket. The
    /// allocated TCP port will be emitted as a tracing event.
    ///
    /// The primary use is live debugging the running child services via tools
    /// that do not support Unix domain sockets (e.g., Prometheus, web
    /// browsers).
    #[clap(long, env = "ORCHESTRATOR_PROCESS_TCP_PROXY_LISTEN_ADDR")]
    orchestrator_process_tcp_proxy_listen_addr: Option<IpAddr>,
    /// A directory in which the process orchestrator should write Prometheus
    /// scrape targets, for use with Prometheus's file-based service discovery.
    ///
    /// Each namespaced orchestrator will maintain a single JSON file into the
    /// directory named `NAMESPACE.json` containing the scrape targets for all
    /// extant services. The scrape targets will use the TCP proxy address, as
    /// Prometheus does not support scraping over Unix domain sockets.
    ///
    /// This option is ignored unless
    /// `--orchestrator-process-tcp-proxy-listen-addr` is set.
    ///
    /// See also: <https://prometheus.io/docs/guides/file-sd/>
    #[clap(
        long,
        env = "ORCHESTRATOR_PROCESS_PROMETHEUS_SERVICE_DISCOVERY_DIRECTORY"
    )]
    orchestrator_process_prometheus_service_discovery_directory: Option<PathBuf>,
    /// The clusterd image reference to use.
    #[structopt(
        long,
        env = "CLUSTERD_IMAGE",
        required_if_eq("orchestrator", "kubernetes"),
        default_value_if("orchestrator", Some("process"), Some("clusterd"))
    )]
    clusterd_image: Option<String>,

    // === Storage options. ===
    /// Where the persist library should store its blob data.
    #[clap(long, env = "PERSIST_BLOB_URL")]
    persist_blob_url: Url,
    /// Where the persist library should perform consensus.
    #[clap(long, env = "PERSIST_CONSENSUS_URL")]
    persist_consensus_url: Url,
    /// The PostgreSQL URL for the storage stash.
    #[clap(long, env = "STORAGE_STASH_URL", value_name = "POSTGRES_URL")]
    storage_stash_url: String,

    // === Adapter options. ===
    /// The PostgreSQL URL for the adapter stash.
    #[clap(long, env = "ADAPTER_STASH_URL", value_name = "POSTGRES_URL")]
    adapter_stash_url: String,

    // === Cloud options. ===
    #[clap(
        long,
        env = "ENVIRONMENT_ID",
        value_name = "<CLOUD>-<REGION>-<ORG-ID>-<ORDINAL>"
    )]
    environment_id: EnvironmentId,
    /// Prefix for an external ID to be supplied to all AWS AssumeRole operations.
    ///
    /// Details: <https://docs.aws.amazon.com/IAM/latest/UserGuide/id_roles_create_for-user_externalid.html>
    #[clap(long, env = "AWS_EXTERNAL_ID_PREFIX", value_name = "ID", parse(from_str = AwsExternalIdPrefix::new_from_cli_argument_or_environment_variable))]
    aws_external_id_prefix: Option<AwsExternalIdPrefix>,
    /// Availability zones in which storage and compute resources may be
    /// deployed.
    #[clap(long, env = "AVAILABILITY_ZONE", use_value_delimiter = true)]
    availability_zone: Vec<String>,
    /// A map from size name to resource allocations for cluster replicas.
    #[clap(
        long,
        env = "CLUSTER_REPLICA_SIZES",
        requires = "bootstrap-default-cluster-replica-size"
    )]
    cluster_replica_sizes: Option<String>,
    /// The size of the default cluster replica if bootstrapping.
    #[clap(
        long,
        env = "BOOTSTRAP_DEFAULT_CLUSTER_REPLICA_SIZE",
        default_value = "1"
    )]
    bootstrap_default_cluster_replica_size: String,
    /// The size of the builtin cluster replicas if bootstrapping.
    #[clap(
        long,
        env = "BOOTSTRAP_BUILTIN_CLUSTER_REPLICA_SIZE",
        default_value = "1"
    )]
    bootstrap_builtin_cluster_replica_size: String,
    /// An list of NAME=VALUE pairs for bootstrapping system parameters that are
    /// not already modified.
    #[clap(
        long,
        env = "BOOTSTRAP_SYSTEM_PARAMETER",
        multiple = true,
        value_delimiter = ';'
    )]
    bootstrap_system_parameter: Vec<KeyValueArg<String, String>>,
    /// Default storage host size
    #[clap(long, env = "DEFAULT_STORAGE_HOST_SIZE")]
    default_storage_host_size: Option<String>,
    /// The interval in seconds at which to collect storage usage information.
    #[clap(
        long,
        env = "STORAGE_USAGE_COLLECTION_INTERVAL",
        parse(try_from_str = humantime::parse_duration),
        default_value = "3600s"
    )]
    storage_usage_collection_interval_sec: Duration,
    /// The period for which to retain usage records. Note that the retention
    /// period is only evaluated at server start time, so rebooting the server
    /// is required to discard old records.
    #[clap(long, env = "STORAGE_USAGE_RETENTION_PERIOD", parse(try_from_str = humantime::parse_duration))]
    storage_usage_retention_period: Option<Duration>,
    /// An API key for Segment. Enables export of audit events to Segment.
    #[clap(long, env = "SEGMENT_API_KEY")]
    segment_api_key: Option<String>,
    /// Public IP addresses which the cloud environment has configured for
    /// egress
    #[clap(
        long,
        env = "ANNOUNCE_EGRESS_IP",
        multiple = true,
        use_delimiter = true
    )]
    announce_egress_ip: Vec<Ipv4Addr>,
    /// An SDK key for LaunchDarkly.
    ///
    /// Setting this in combination with [`Self::config_sync_loop_interval`]
    /// will enable synchronization of LaunchDarkly features with system
    /// configuration parameters.
    #[clap(long, env = "LAUNCHDARKLY_SDK_KEY")]
    launchdarkly_sdk_key: Option<String>,
    /// A list of PARAM_NAME=KEY_NAME pairs from system parameter names to
    /// LaunchDarkly feature keys.
    ///
    /// This is used (so far only for testing purposes) when propagating values
    /// from the latter to the former. The identity map is assumed for absent
    /// parameter names.
    #[clap(
        long,
        env = "LAUNCHDARKLY_KEY_MAP",
        multiple = true,
        value_delimiter = ';'
    )]
    launchdarkly_key_map: Vec<KeyValueArg<String, String>>,
    /// The interval in seconds at which to synchronize system parameter values.
    ///
    /// If this is not explicitly set, the loop that synchronizes LaunchDarkly
    /// features with system configuration parameters will not run _even if
    /// [`Self::launchdarkly_sdk_key`] is present_.
    #[clap(
        long,
        env = "CONFIG_SYNC_LOOP_INTERVAL",
        parse(try_from_str = humantime::parse_duration),
    )]
    config_sync_loop_interval: Option<Duration>,

    /// The 12-digit AWS account id, which is used to generate an AWS Principal.
    #[clap(long, env = "AWS_ACCOUNT_ID")]
    aws_account_id: Option<String>,

    /// The list of supported AWS PrivateLink availability zone ids.
    /// Must be zone IDs, of format e.g. "use-az1".
    #[clap(
        long,
        env = "AWS_PRIVATELINK_AVAILABILITY_ZONES",
        multiple = true,
        use_delimiter = true
    )]
    aws_privatelink_availability_zones: Option<Vec<String>>,

    // === Tracing options. ===
    #[clap(flatten)]
    tracing: TracingCliArgs,
}

#[derive(ArgEnum, Debug, Clone)]
enum OrchestratorKind {
    Kubernetes,
    Process,
}

fn main() {
    let args = cli::parse_args(CliConfig {
        env_prefix: Some("MZ_"),
        enable_version_flag: true,
    });
    if let Err(err) = run(args) {
        eprintln!("environmentd: {:#}", err);
        process::exit(1);
    }
}

fn run(mut args: Args) -> Result<(), anyhow::Error> {
    mz_ore::panic::set_abort_on_panic();
    let envd_start = Instant::now();

    // Configure signal handling as soon as possible. We want signals to be
    // handled to our liking ASAP.
    sys::enable_sigusr2_coverage_dump()?;
    sys::enable_termination_signal_cleanup()?;

    // Start Tokio runtime.

    let ncpus_useful = usize::max(1, cmp::min(num_cpus::get(), num_cpus::get_physical()));
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(ncpus_useful)
            // The default thread name exceeds the Linux limit on thread name
            // length, so pick something shorter.
            .thread_name_fn(|| {
                static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
                let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
                format!("tokio:work-{}", id)
            })
            .enable_all()
            .build()?,
    );

    // Configure tracing to log the service name when using the process
    // orchestrator, which intermingles log output from multiple services. Other
    // orchestrators separate log output from different services.
    args.tracing.log_prefix = if matches!(args.orchestrator, OrchestratorKind::Process) {
        Some("environmentd".to_string())
    } else {
        None
    };
    let (tracing_handle, _tracing_guard) =
        runtime.block_on(args.tracing.configure_tracing(StaticTracingConfig {
            service_name: "environmentd",
            build_info: BUILD_INFO,
        }))?;

    let span = tracing::info_span!("environmentd::run").entered();

    let metrics_registry = MetricsRegistry::new();
    let metrics = Metrics::register_into(&metrics_registry);

    runtime.block_on(mz_alloc::register_metrics_into(&metrics_registry));

    // Initialize fail crate for failpoint support
    let _failpoint_scenario = FailScenario::setup();

    // Configure connections.
    let tls = if args.tls_mode == "disable" {
        if args.tls_cert.is_some() {
            bail!("cannot specify --tls-mode=disable and --tls-cert simultaneously");
        }
        if args.tls_key.is_some() {
            bail!("cannot specify --tls-mode=disable and --tls-key simultaneously");
        }
        None
    } else {
        let mode = match args.tls_mode.as_str() {
            "require" => TlsMode::Require,
            _ => unreachable!(),
        };
        let cert = args.tls_cert.unwrap();
        let key = args.tls_key.unwrap();
        Some(TlsConfig { mode, cert, key })
    };
    let frontegg = match (
        args.frontegg_tenant,
        args.frontegg_api_token_url,
        args.frontegg_jwk,
        args.frontegg_admin_role,
    ) {
        (None, None, None, None) => None,
        (Some(tenant_id), Some(admin_api_token_url), Some(jwk), Some(admin_role)) => {
            Some(FronteggAuthentication::new(
                FronteggConfig {
                    admin_api_token_url,
                    decoding_key: DecodingKey::from_rsa_pem(jwk.as_bytes())?,
                    tenant_id,
                    now: mz_ore::now::SYSTEM_TIME.clone(),
                    refresh_before_secs: 60,
                    admin_role,
                },
                mz_frontegg_auth::Client::environmentd_default(),
            ))
        }
        _ => unreachable!("clap enforced"),
    };

    // Configure CORS.
    let allowed_origins = if !args.cors_allowed_origin.is_empty() {
        args.cors_allowed_origin
    } else {
        let port = args.http_listen_addr.port();
        vec![
            HeaderValue::from_str(&format!("http://localhost:{}", port)).unwrap(),
            HeaderValue::from_str(&format!("http://127.0.0.1:{}", port)).unwrap(),
            HeaderValue::from_str(&format!("http://[::1]:{}", port)).unwrap(),
            HeaderValue::from_str(&format!("https://localhost:{}", port)).unwrap(),
            HeaderValue::from_str(&format!("https://127.0.0.1:{}", port)).unwrap(),
            HeaderValue::from_str(&format!("https://[::1]:{}", port)).unwrap(),
        ]
    };
    let cors_allowed_origin = mz_http_util::build_cors_allowed_origin(&allowed_origins);

    // Configure controller.
    let (orchestrator, secrets_controller, cloud_resource_controller): (
        Arc<dyn Orchestrator>,
        Arc<dyn SecretsController>,
        Option<Arc<dyn CloudResourceController>>,
    ) = match args.orchestrator {
        OrchestratorKind::Kubernetes => {
            let orchestrator = Arc::new(
                runtime
                    .block_on(KubernetesOrchestrator::new(KubernetesOrchestratorConfig {
                        context: args.orchestrator_kubernetes_context.clone(),
                        scheduler_name: args.orchestrator_kubernetes_scheduler_name,
                        service_labels: args
                            .orchestrator_kubernetes_service_label
                            .into_iter()
                            .map(|l| (l.key, l.value))
                            .collect(),
                        service_node_selector: args
                            .orchestrator_kubernetes_service_node_selector
                            .into_iter()
                            .map(|l| (l.key, l.value))
                            .collect(),
                        service_account: args.orchestrator_kubernetes_service_account,
                        image_pull_policy: args.orchestrator_kubernetes_image_pull_policy,
                        aws_external_id_prefix: args.aws_external_id_prefix.clone(),
                    }))
                    .context("creating kubernetes orchestrator")?,
            );
            let secrets_controller = Arc::clone(&orchestrator);
            let cloud_resource_controller = Arc::clone(&orchestrator);
            (
                orchestrator,
                secrets_controller,
                Some(cloud_resource_controller),
            )
        }
        OrchestratorKind::Process => {
            let orchestrator = Arc::new(
                runtime
                    .block_on(ProcessOrchestrator::new(ProcessOrchestratorConfig {
                        // Look for binaries in the same directory as the
                        // running binary. When running via `cargo run`, this
                        // means that debug binaries look for other debug
                        // binaries and release binaries look for other release
                        // binaries.
                        image_dir: env::current_exe()?.parent().unwrap().to_path_buf(),
                        suppress_output: false,
                        environment_id: args.environment_id.to_string(),
                        secrets_dir: args
                            .orchestrator_process_secrets_directory
                            .expect("clap enforced"),
                        command_wrapper: args
                            .orchestrator_process_wrapper
                            .map_or(Ok(vec![]), |s| shell_words::split(&s))?,
                        propagate_crashes: args.orchestrator_process_propagate_crashes,
                        tcp_proxy: args.orchestrator_process_tcp_proxy_listen_addr.map(
                            |listen_addr| ProcessOrchestratorTcpProxyConfig {
                                listen_addr,
                                prometheus_service_discovery_dir: args
                                    .orchestrator_process_prometheus_service_discovery_directory,
                            },
                        ),
                    }))
                    .context("creating process orchestrator")?,
            );
            let secrets_controller = Arc::clone(&orchestrator);
            (orchestrator, secrets_controller, None)
        }
    };
    let secrets_reader = secrets_controller.reader();
    let now = SYSTEM_TIME.clone();
    let persist_clients = PersistClientCache::new(
        PersistConfig::new(&mz_environmentd::BUILD_INFO, now.clone()),
        &metrics_registry,
    );
    let persist_clients = Arc::new(persist_clients);
    let orchestrator = Arc::new(TracingOrchestrator::new(orchestrator, args.tracing.clone()));
    let controller = ControllerConfig {
        build_info: &mz_environmentd::BUILD_INFO,
        orchestrator,
        persist_location: PersistLocation {
            blob_uri: args.persist_blob_url.to_string(),
            consensus_uri: args.persist_consensus_url.to_string(),
        },
        persist_clients,
        storage_stash_url: args.storage_stash_url,
        clusterd_image: args.clusterd_image.expect("clap enforced"),
        init_container_image: args.orchestrator_kubernetes_init_container_image,
        now: SYSTEM_TIME.clone(),
        postgres_factory: StashFactory::new(&metrics_registry),
        metrics_registry: metrics_registry.clone(),
    };

    let cluster_replica_sizes: ClusterReplicaSizeMap = match args.cluster_replica_sizes {
        None => Default::default(),
        Some(json) => serde_json::from_str(&json).context("parsing replica size map")?,
    };

    // Ensure default storage cluster size actually exists in the passed map
    if let Some(default_storage_cluster_size) = &args.default_storage_host_size {
        if !cluster_replica_sizes
            .0
            .contains_key(default_storage_cluster_size)
        {
            bail!("default storage cluster size is unknown");
        }
    }

    emit_boot_diagnostics!(&BUILD_INFO);
    sys::adjust_rlimits();

    let server = runtime.block_on(mz_environmentd::serve(mz_environmentd::Config {
        sql_listen_addr: args.sql_listen_addr,
        http_listen_addr: args.http_listen_addr,
        internal_sql_listen_addr: args.internal_sql_listen_addr,
        internal_http_listen_addr: args.internal_http_listen_addr,
        tls,
        frontegg,
        cors_allowed_origin,
        adapter_stash_url: args.adapter_stash_url,
        controller,
        secrets_controller,
        cloud_resource_controller,
        unsafe_mode: args.unsafe_mode,
        metrics_registry,
        now,
        environment_id: args.environment_id,
        cluster_replica_sizes,
        default_storage_cluster_size: args.default_storage_host_size,
        bootstrap_default_cluster_replica_size: args.bootstrap_default_cluster_replica_size,
        bootstrap_builtin_cluster_replica_size: args.bootstrap_builtin_cluster_replica_size,
        bootstrap_system_parameters: args
            .bootstrap_system_parameter
            .into_iter()
            .map(|kv| (kv.key, kv.value))
            .collect(),
        availability_zones: args.availability_zone,
        connection_context: ConnectionContext::from_cli_args(
            &args.tracing.log_filter.inner,
            args.aws_external_id_prefix,
            secrets_reader,
        ),
        tracing_handle,
        storage_usage_collection_interval: args.storage_usage_collection_interval_sec,
        storage_usage_retention_period: args.storage_usage_retention_period,
        segment_api_key: args.segment_api_key,
        egress_ips: args.announce_egress_ip,
        aws_account_id: args.aws_account_id,
        aws_privatelink_availability_zones: args.aws_privatelink_availability_zones,
        launchdarkly_sdk_key: args.launchdarkly_sdk_key,
        launchdarkly_key_map: args
            .launchdarkly_key_map
            .into_iter()
            .map(|kv| (kv.key, kv.value))
            .collect(),
        config_sync_loop_interval: args.config_sync_loop_interval,
    }))?;

    metrics.start_time_environmentd.set(
        envd_start
            .elapsed()
            .as_millis()
            .try_into()
            .expect("must fit"),
    );
    let span = span.exit();
    let id = span.context().span().span_context().trace_id();
    drop(span);

    println!(
        "environmentd {} listening...",
        mz_environmentd::BUILD_INFO.human_version()
    );
    println!(" SQL address: {}", server.sql_local_addr());
    println!(" HTTP address: {}", server.http_local_addr());
    println!(
        " Internal SQL address: {}",
        server.internal_sql_local_addr()
    );
    println!(
        " Internal HTTP address: {}",
        server.internal_http_local_addr()
    );

    println!(" Root trace ID: {id}");

    // Block forever.
    loop {
        thread::park();
    }
}

fn build_info() -> Vec<String> {
    let openssl_version =
        unsafe { CStr::from_ptr(openssl_sys::OpenSSL_version(openssl_sys::OPENSSL_VERSION)) };
    let rdkafka_version = unsafe { CStr::from_ptr(rdkafka_sys::bindings::rd_kafka_version_str()) };
    vec![
        openssl_version.to_string_lossy().into_owned(),
        format!("librdkafka v{}", rdkafka_version.to_string_lossy()),
    ]
}

#[derive(Debug, Clone)]
struct Metrics {
    pub start_time_environmentd: IntGauge,
}

impl Metrics {
    pub fn register_into(registry: &MetricsRegistry) -> Metrics {
        Metrics {
            start_time_environmentd: registry.register(metric!(
                name: "mz_start_time_environmentd",
                help: "Time in milliseconds from environmentd start until the adapter is ready.",
            )),
        }
    }
}
