mod commands;
mod runtime;
mod start;
mod tenant;
mod up;

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "defraburner",
    version,
    about = "DefraDB cluster in one binary: cells, P2P mesh, tenants, autoscaling, dashboard"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bring the cluster up: recover it from `data_root` if it already
    /// exists, else provision a fresh single cell, then print a
    /// ready-to-use dashboard URL and open it in a browser (D21: this is
    /// what bare `defraburner` runs).
    Up {
        /// Root directory for cluster and cell data. Defaults to
        /// `$DEFRABURNER_DATA`, then `$HOME/.local/share/defraburner`.
        #[arg(long)]
        data_root: Option<PathBuf>,
        /// Bind address for a freshly-provisioned cell's libp2p transport.
        #[arg(long, default_value = "127.0.0.1")]
        bind: IpAddr,
        /// Static cross-host peer multiaddrs to dial once cells are up
        /// (comma-separated; each must carry a `/p2p/<peer-id>` suffix).
        #[arg(long, value_delimiter = ',')]
        peers: Vec<String>,
        /// Listen address for the gateway: tenant routing, admission,
        /// and the `/admin/*` endpoints (including the dashboard).
        #[arg(long, default_value = burner_gateway::gateway::DEFAULT_GATEWAY_ADDR)]
        gateway_addr: SocketAddr,
        /// If given, atomically write cluster status here (JSON) once
        /// every cell is up.
        #[arg(long)]
        ready_file: Option<PathBuf>,
        /// Minimum cells the autoscaler will ever hold the fleet at.
        #[arg(long, default_value_t = 1)]
        min_cells: usize,
        /// Maximum cells the autoscaler will ever scale up to.
        #[arg(long, default_value_t = 8)]
        max_cells: usize,
        /// Minimum seconds between two autoscaler-executed scale actions.
        #[arg(long, default_value_t = 60)]
        cooldown_secs: u64,
        /// Seconds between autoscaler control-loop ticks.
        #[arg(long = "tick-interval", default_value_t = 5)]
        tick_interval_secs: u64,
        /// Directory of burn policy package overrides: each direct
        /// subdirectory containing a `*.afb` archive overrides the
        /// embedded default policy of the same directory name. Omitted:
        /// only the embedded defaults (autoscale-default,
        /// placement-default) run.
        #[arg(long)]
        packages_dir: Option<PathBuf>,
        /// Skip the best-effort browser open.
        #[arg(long)]
        no_open: bool,
        /// Fuel ceiling (afterburner's own instruction budget) per policy
        /// call. Omitted: afterburner's own default (unlimited).
        #[arg(long = "policy-fuel")]
        policy_fuel: Option<u64>,
        /// Linear memory ceiling, in bytes, for the shared policy engine.
        /// Omitted: afterburner's own default (unlimited).
        #[arg(long = "policy-memory-bytes")]
        policy_memory_bytes: Option<usize>,
        /// Wall-clock timeout ceiling, in milliseconds, per policy call.
        /// Omitted: afterburner's own default (unlimited).
        #[arg(long = "policy-timeout-ms")]
        policy_timeout_ms: Option<u64>,
    },
    /// Provision (or recover) a cluster of governed cells and serve it
    /// until SIGINT/SIGTERM.
    Start {
        /// Root directory for cluster and cell data. Created if missing.
        #[arg(long, default_value = "./data")]
        data_root: PathBuf,
        /// Number of cells to provision. Only used when `data_root` has no
        /// existing cluster manifest; an existing cluster is recovered as
        /// recorded, regardless of this value.
        #[arg(long, default_value_t = 2)]
        cells: usize,
        /// Bind address for freshly-provisioned cells' libp2p transport.
        #[arg(long, default_value = "127.0.0.1")]
        bind: IpAddr,
        /// First libp2p port for freshly-provisioned cells; cell N gets
        /// `base_port + N`.
        #[arg(long, default_value_t = 9171)]
        base_port: u16,
        /// Static cross-host peer multiaddrs to dial once cells are up
        /// (comma-separated; each must carry a `/p2p/<peer-id>` suffix).
        #[arg(long, value_delimiter = ',')]
        peers: Vec<String>,
        /// Listen address for the gateway (Phase 3): tenant routing,
        /// admission, and the `/admin/*` endpoints.
        #[arg(long, default_value = burner_gateway::gateway::DEFAULT_GATEWAY_ADDR)]
        gateway_addr: SocketAddr,
        /// If given, atomically write cluster status here (JSON) once every
        /// cell is up.
        #[arg(long)]
        ready_file: Option<PathBuf>,
        /// Minimum cells the autoscaler (Phase 4) will ever hold the fleet
        /// at.
        #[arg(long, default_value_t = 1)]
        min_cells: usize,
        /// Maximum cells the autoscaler (Phase 4) will ever scale up to.
        #[arg(long, default_value_t = 8)]
        max_cells: usize,
        /// Minimum seconds between two autoscaler-executed scale actions.
        #[arg(long, default_value_t = 60)]
        cooldown_secs: u64,
        /// Seconds between autoscaler control-loop ticks.
        #[arg(long = "tick-interval", default_value_t = 5)]
        tick_interval_secs: u64,
        /// Directory of burn policy package overrides (Phase 4, D9/D17a):
        /// each direct subdirectory containing a `*.afb` archive overrides
        /// the embedded default policy of the same directory name.
        /// Omitted: only the embedded defaults (autoscale-default,
        /// placement-default) run.
        #[arg(long)]
        packages_dir: Option<PathBuf>,
        /// Fuel ceiling (afterburner's own instruction budget) per policy
        /// call. Omitted: afterburner's own default (unlimited).
        #[arg(long = "policy-fuel")]
        policy_fuel: Option<u64>,
        /// Linear memory ceiling, in bytes, for the shared policy engine.
        /// Omitted: afterburner's own default (unlimited).
        #[arg(long = "policy-memory-bytes")]
        policy_memory_bytes: Option<usize>,
        /// Wall-clock timeout ceiling, in milliseconds, per policy call.
        /// Omitted: afterburner's own default (unlimited).
        #[arg(long = "policy-timeout-ms")]
        policy_timeout_ms: Option<u64>,
    },
    /// Print the cluster manifest at `data_root` as pretty JSON, without
    /// igniting any cell. Offline inspection only.
    Status {
        /// Root directory for cluster and cell data.
        #[arg(long, default_value = "./data")]
        data_root: PathBuf,
    },
    /// Manage tenants (Phase 2, D14: declarative provisioning). Offline:
    /// edits the cluster manifest without igniting any cell; placement
    /// happens on the next `start`.
    Tenant {
        #[command(subcommand)]
        action: TenantCommand,
    },
}

#[derive(Subcommand)]
enum TenantCommand {
    /// Validate a schema, copy it into the data root, and record a
    /// `Pending` tenant in the cluster manifest.
    Create {
        /// Root directory for cluster and cell data.
        #[arg(long, default_value = "./data")]
        data_root: PathBuf,
        /// Tenant name: `[a-z0-9-]{1,63}`.
        #[arg(long)]
        name: String,
        /// Path to the tenant's GraphQL SDL schema file.
        #[arg(long)]
        schema: PathBuf,
        /// Replication factor: how many cells the tenant's group spans.
        #[arg(long, default_value_t = 2)]
        replicas: u8,
    },
    /// Print the cluster manifest's tenants as pretty JSON.
    List {
        /// Root directory for cluster and cell data.
        #[arg(long, default_value = "./data")]
        data_root: PathBuf,
    },
    /// Issue a fresh bearer token for an existing tenant (Phase 3),
    /// replacing its previous token. Printed once.
    RotateToken {
        /// Root directory for cluster and cell data.
        #[arg(long, default_value = "./data")]
        data_root: PathBuf,
        /// Tenant name.
        #[arg(long)]
        name: String,
    },
}

/// Every top-level subcommand name [`inject_default_subcommand`] must
/// never splice `"up"` in front of. `"help"` is included because clap
/// treats it as a real subcommand (`defraburner help start` works),
/// distinct from the `-h`/`--help` flags handled separately below.
const KNOWN_SUBCOMMANDS: &[&str] = &["up", "start", "status", "tenant", "help"];
/// Flags that must reach clap completely unmodified so the *root*
/// help/version text prints -- splicing `"up"` in front of `--help`
/// would print `up`'s own help instead of the subcommand list.
const HELP_VERSION_FLAGS: &[&str] = &["-h", "--help", "-V", "--version"];

/// Pre-parse argv splicing (D21): bare `defraburner` (or `defraburner
/// <some-up-flag>`) should behave like `defraburner up <some-up-flag>`.
/// Inserts `"up"` right after `argv[0]` when `argv[1]` is absent, an
/// unknown token (not a recognized subcommand), or a flag that is not
/// `-h`/`--help`/`-V`/`--version`. Never touches `argv` when `argv[1]`
/// already names a known subcommand or one of those four flags.
///
/// Pure and total: never panics, never touches the environment, always
/// returns a `Vec` clap can parse (a genuinely malformed remainder, e.g.
/// `defraburner --bogus-flag`, still gets `"up"` spliced in front of it;
/// clap itself then reports the unrecognized flag against `up`'s own
/// argument set, which is the more actionable error).
fn inject_default_subcommand(argv: Vec<String>) -> Vec<String> {
    let Some(first_arg) = argv.get(1) else {
        let mut spliced = argv;
        spliced.push("up".to_string());
        return spliced;
    };

    if HELP_VERSION_FLAGS.contains(&first_arg.as_str())
        || KNOWN_SUBCOMMANDS.contains(&first_arg.as_str())
    {
        return argv;
    }

    let mut spliced = Vec::with_capacity(argv.len() + 1);
    let mut iter = argv.into_iter();
    spliced.push(
        iter.next()
            .expect("argv[0] exists: argv.get(1) was Some above"),
    );
    spliced.push("up".to_string());
    spliced.extend(iter);
    spliced
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let argv = inject_default_subcommand(std::env::args().collect());
    let cli = Cli::parse_from(argv);

    match cli.command {
        Command::Up {
            data_root,
            bind,
            peers,
            gateway_addr,
            ready_file,
            min_cells,
            max_cells,
            cooldown_secs,
            tick_interval_secs,
            packages_dir,
            no_open,
            policy_fuel,
            policy_memory_bytes,
            policy_timeout_ms,
        } => {
            let data_root = up::resolve_data_root(data_root)?;
            // Fresh-provision only: a recovered cluster ignores this and
            // reuses each cell's own recorded p2p_port from the manifest
            // (see `start::run`'s recover-vs-fresh-provision branch).
            let base_port = up::find_free_port(9171, 64)?;
            let open_browser = up::should_open_browser(no_open);
            let policy_limits = runtime::RuntimeLimits {
                fuel: policy_fuel,
                memory_bytes: policy_memory_bytes,
                timeout_ms: policy_timeout_ms,
            };
            start::run(
                data_root,
                1,
                bind,
                base_port,
                peers,
                gateway_addr,
                ready_file,
                min_cells,
                max_cells,
                cooldown_secs,
                std::time::Duration::from_secs(tick_interval_secs),
                packages_dir,
                policy_limits,
                Some(start::AnnounceOptions { open_browser }),
            )
            .await?;
        }
        Command::Start {
            data_root,
            cells,
            bind,
            base_port,
            peers,
            gateway_addr,
            ready_file,
            min_cells,
            max_cells,
            cooldown_secs,
            tick_interval_secs,
            packages_dir,
            policy_fuel,
            policy_memory_bytes,
            policy_timeout_ms,
        } => {
            let policy_limits = runtime::RuntimeLimits {
                fuel: policy_fuel,
                memory_bytes: policy_memory_bytes,
                timeout_ms: policy_timeout_ms,
            };
            start::run(
                data_root,
                cells,
                bind,
                base_port,
                peers,
                gateway_addr,
                ready_file,
                min_cells,
                max_cells,
                cooldown_secs,
                std::time::Duration::from_secs(tick_interval_secs),
                packages_dir,
                policy_limits,
                None,
            )
            .await?;
        }
        Command::Status { data_root } => {
            let manifest = burner_cell::ClusterManifest::load(&data_root)
                .await
                .with_context(|| {
                    format!("loading cluster manifest from {}", data_root.display())
                })?;
            println!("{}", serde_json::to_string_pretty(&manifest)?);
        }
        Command::Tenant { action } => match action {
            TenantCommand::Create {
                data_root,
                name,
                schema,
                replicas,
            } => {
                tenant::create(data_root, name, schema, replicas).await?;
            }
            TenantCommand::List { data_root } => {
                tenant::list(data_root).await?;
            }
            TenantCommand::RotateToken { data_root, name } => {
                tenant::rotate_token(data_root, name).await?;
            }
        },
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_invocation_injects_up() {
        assert_eq!(
            inject_default_subcommand(argv(&["defraburner"])),
            argv(&["defraburner", "up"])
        );
    }

    #[test]
    fn known_subcommands_are_never_touched() {
        for known in KNOWN_SUBCOMMANDS {
            let input = argv(&["defraburner", known]);
            assert_eq!(
                inject_default_subcommand(input.clone()),
                input,
                "'{known}' should pass through unmodified"
            );
        }
    }

    #[test]
    fn a_known_subcommand_with_its_own_flags_is_untouched() {
        let input = argv(&["defraburner", "start", "--cells", "3"]);
        assert_eq!(inject_default_subcommand(input.clone()), input);
    }

    #[test]
    fn help_and_version_flags_are_never_touched() {
        for flag in HELP_VERSION_FLAGS {
            let input = argv(&["defraburner", flag]);
            assert_eq!(
                inject_default_subcommand(input.clone()),
                input,
                "'{flag}' should pass through unmodified so root help/version prints"
            );
        }
    }

    #[test]
    fn an_up_flag_without_the_up_word_gets_it_spliced_in() {
        assert_eq!(
            inject_default_subcommand(argv(&["defraburner", "--no-open"])),
            argv(&["defraburner", "up", "--no-open"])
        );
        assert_eq!(
            inject_default_subcommand(argv(&["defraburner", "--data-root", "/tmp/x"])),
            argv(&["defraburner", "up", "--data-root", "/tmp/x"])
        );
    }

    #[test]
    fn an_unknown_subcommand_word_gets_up_spliced_in_front() {
        assert_eq!(
            inject_default_subcommand(argv(&["defraburner", "bogus"])),
            argv(&["defraburner", "up", "bogus"])
        );
    }

    #[test]
    fn only_argv1_is_ever_inspected_not_later_positions() {
        // "start" appearing later than position 1 must not itself prevent
        // injection: this exercises that only argv[1] drives the
        // decision, not a scan of the whole argv.
        assert_eq!(
            inject_default_subcommand(argv(&["defraburner", "--peers", "start"])),
            argv(&["defraburner", "up", "--peers", "start"])
        );
    }
}
