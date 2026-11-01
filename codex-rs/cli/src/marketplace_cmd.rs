use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use clap::Parser;
use codex_core::config::Config;
use codex_core::config::find_codex_home;
use codex_core_plugins::OPENAI_BUNDLED_MARKETPLACE_NAME;
use codex_core_plugins::PluginMarketplaceUpgradeOutcome;
use codex_core_plugins::PluginsConfigInput;
use codex_core_plugins::PluginsManager;
use codex_core_plugins::allowed_configured_marketplace_names;
use codex_core_plugins::installed_marketplaces::marketplace_install_root;
use codex_core_plugins::installed_marketplaces::resolve_configured_marketplace_root;
use codex_core_plugins::marketplace::MarketplaceListError;
use codex_core_plugins::marketplace::find_marketplace_manifest_path;
use codex_core_plugins::marketplace::marketplace_root_dir;
use codex_core_plugins::marketplace_add::MarketplaceAddOutcome;
use codex_core_plugins::marketplace_add::MarketplaceAddRequest;
use codex_core_plugins::marketplace_add::add_marketplace;
use codex_core_plugins::marketplace_remove::MarketplaceRemoveOutcome;
use codex_core_plugins::marketplace_remove::MarketplaceRemoveRequest;
use codex_core_plugins::marketplace_remove::remove_marketplace;
use codex_login::CodexAuth;
use codex_login::auth::read_codex_api_key_from_env;
use codex_plugin::validate_plugin_segment;
use codex_protocol::auth::AuthMode;
use codex_utils_cli::CliConfigOverrides;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(bin_name = "codex plugin marketplace")]
pub struct MarketplaceCli {
    #[clap(flatten)]
    pub config_overrides: CliConfigOverrides,

    #[command(subcommand)]
    subcommand: MarketplaceSubcommand,
}

#[derive(Debug, clap::Subcommand)]
enum MarketplaceSubcommand {
    /// Add a local or Git marketplace to the configured marketplace sources.
    Add(AddMarketplaceArgs),

    /// List plugin marketplaces Codex is currently considering and their roots.
    List(ListMarketplaceArgs),

    /// Refresh configured Git marketplace snapshots.
    ///
    /// Omit MARKETPLACE_NAME to upgrade all configured Git marketplaces.
    Upgrade(UpgradeMarketplaceArgs),

    /// Remove a configured marketplace source by name.
    Remove(RemoveMarketplaceArgs),
}

#[derive(Debug, Parser)]
#[command(
    bin_name = "codex plugin marketplace add",
    after_help = "Examples:\n  codex plugin marketplace add ./path/to/marketplace\n  codex plugin marketplace add owner/repo --ref main\n  codex plugin marketplace add https://github.com/owner/repo --sparse plugins/foo"
)]
struct AddMarketplaceArgs {
    /// Marketplace source: a local path, owner/repo[@ref], HTTPS Git URL, or SSH Git URL.
    #[arg(value_name = "SOURCE")]
    source: String,

    /// Git ref to fetch for Git marketplace sources.
    #[arg(long = "ref", value_name = "REF")]
    ref_name: Option<String>,

    /// Sparse checkout path for Git marketplace sources. Can be repeated.
    #[arg(
        long = "sparse",
        value_name = "PATH",
        action = clap::ArgAction::Append
    )]
    sparse_paths: Vec<String>,

    /// Output add result as JSON.
    #[arg(long = "json")]
    json: bool,
}

#[derive(Debug, Parser)]
#[command(bin_name = "codex plugin marketplace list")]
struct ListMarketplaceArgs {
    /// Output marketplace list as JSON.
    #[arg(long = "json")]
    json: bool,
}

#[derive(Debug, Parser)]
#[command(
    bin_name = "codex plugin marketplace upgrade",
    after_help = "Examples:\n  codex plugin marketplace upgrade\n  codex plugin marketplace upgrade debug"
)]
struct UpgradeMarketplaceArgs {
    /// Optional configured marketplace name to upgrade. Omit to upgrade all Git marketplaces.
    #[arg(value_name = "MARKETPLACE_NAME")]
    marketplace_name: Option<String>,

    /// Output upgrade result as JSON.
    #[arg(long = "json")]
    json: bool,
}

#[derive(Debug, Parser)]
#[command(
    bin_name = "codex plugin marketplace remove",
    after_help = "Example:\n  codex plugin marketplace remove debug"
)]
struct RemoveMarketplaceArgs {
    /// Configured marketplace name to remove.
    #[arg(value_name = "MARKETPLACE_NAME")]
    marketplace_name: String,

    /// Output remove result as JSON.
    #[arg(long = "json")]
    json: bool,
}

impl MarketplaceCli {
    pub async fn run(self) -> Result<()> {
        let MarketplaceCli {
            config_overrides,
            subcommand,
        } = self;

        let overrides = config_overrides
            .parse_overrides()
            .map_err(anyhow::Error::msg)?;

        match subcommand {
            MarketplaceSubcommand::Add(args) => run_add(overrides, args).await?,
            MarketplaceSubcommand::List(args) => run_list(overrides, args).await?,
            MarketplaceSubcommand::Upgrade(args) => run_upgrade(overrides, args).await?,
            MarketplaceSubcommand::Remove(args) => run_remove(overrides, args).await?,
        }

        Ok(())
    }
}

async fn run_add(overrides: Vec<(String, toml::Value)>, args: AddMarketplaceArgs) -> Result<()> {
    let AddMarketplaceArgs {
        source,
        ref_name,
        sparse_paths,
        json,
    } = args;

    let config = Config::load_with_cli_overrides(overrides)
        .await
        .context("failed to load configuration")?;
    let outcome = add_marketplace(
        config.codex_home.to_path_buf(),
        config.config_layer_stack.requirements().clone(),
        MarketplaceAddRequest {
            source,
            ref_name,
            sparse_paths,
        },
    )
    .await?;

    if json {
        let output = JsonMarketplaceAddOutput::from_outcome(outcome);
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if outcome.already_added {
        println!(
            "Marketplace `{}` is already added from {}.",
            outcome.marketplace_name, outcome.source_display
        );
    } else {
        println!(
            "Added marketplace `{}` from {}.",
            outcome.marketplace_name, outcome.source_display
        );
    }
    println!(
        "Installed marketplace root: {}",
        outcome.installed_root.as_path().display()
    );

    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonMarketplaceAddOutput {
    marketplace_name: String,
    installed_root: String,
    already_added: bool,
}

impl JsonMarketplaceAddOutput {
    fn from_outcome(outcome: MarketplaceAddOutcome) -> Self {
        Self {
            marketplace_name: outcome.marketplace_name,
            installed_root: outcome.installed_root.as_path().display().to_string(),
            already_added: outcome.already_added,
        }
    }
}

async fn run_list(overrides: Vec<(String, toml::Value)>, args: ListMarketplaceArgs) -> Result<()> {
    let config = Config::load_with_cli_overrides(overrides)
        .await
        .context("failed to load configuration")?;
    let manager = PluginsManager::new(config.codex_home.to_path_buf());
    manager.set_auth_mode(load_cli_auth_mode(&config).await);
    let plugins_input = config.plugins_config_input();
    let marketplace_listing = manager
        .discover_marketplaces_for_config(&plugins_input, &[])
        .context("failed to list plugin marketplaces")?;
    let mut load_issues = configured_marketplace_snapshot_issues(
        config.codex_home.as_path(),
        &plugins_input,
        &marketplace_listing.errors,
        /*marketplace_name*/ None,
    );
    let mut issue_paths = load_issues
        .iter()
        .map(|issue| issue.path.clone())
        .collect::<HashSet<_>>();
    for error in &marketplace_listing.errors {
        if issue_paths.insert(error.path.to_path_buf()) {
            load_issues.push(ConfiguredMarketplaceSnapshotIssue {
                marketplace_name: error.path.display().to_string(),
                path: error.path.to_path_buf(),
                message: error.message.clone(),
            });
        }
    }
    if !load_issues.is_empty() {
        let issue_lines = load_issues
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        bail!("failed to load marketplace(s):\n{issue_lines}");
    }
    let marketplaces = marketplace_listing.marketplaces;
    if args.json {
        let marketplace_sources =
            configured_marketplace_sources_by_root(config.codex_home.as_path(), &plugins_input);
        let output =
            JsonMarketplaceListOutput::from_marketplaces(marketplaces, &marketplace_sources);
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if marketplaces.is_empty() {
        println!("No plugin marketplaces in scope.");
        return Ok(());
    }

    let mut seen_roots = HashSet::new();
    let mut rows = Vec::new();
    for marketplace in marketplaces {
        let Ok(root) = marketplace_root_dir(&marketplace.path) else {
            continue;
        };
        if !seen_roots.insert(root.clone()) {
            continue;
        }
        rows.push((marketplace.name, root));
    }

    let marketplace_width = rows
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or("MARKETPLACE".len())
        .max("MARKETPLACE".len());

    println!("{:<marketplace_width$}  ROOT", "MARKETPLACE");
    for (marketplace_name, root) in rows {
        println!(
            "{:<marketplace_width$}  {}",
            marketplace_name,
            root.display()
        );
    }

    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonMarketplaceListOutput {
    marketplaces: Vec<JsonMarketplaceListEntry>,
}

impl JsonMarketplaceListOutput {
    fn from_marketplaces(
        marketplaces: Vec<codex_core_plugins::marketplace::Marketplace>,
        marketplace_sources: &HashMap<PathBuf, JsonMarketplaceSource>,
    ) -> Self {
        let mut seen_roots = HashSet::new();
        let marketplaces = marketplaces
            .into_iter()
            .filter_map(|marketplace| {
                let root = marketplace_root_dir(&marketplace.path).ok()?;
                if !seen_roots.insert(root.clone()) {
                    return None;
                }
                Some(JsonMarketplaceListEntry {
                    marketplace_source: marketplace_sources.get(root.as_path()).cloned(),
                    name: marketplace.name,
                    root: root.display().to_string(),
                })
            })
            .collect();

        Self { marketplaces }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonMarketplaceListEntry {
    name: String,
    root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    marketplace_source: Option<JsonMarketplaceSource>,
}

fn configured_marketplace_sources_by_root(
    codex_home: &Path,
    plugins_input: &PluginsConfigInput,
) -> HashMap<PathBuf, JsonMarketplaceSource> {
    let marketplace_sources = configured_marketplace_sources(plugins_input, codex_home);
    let user_config = plugins_input.config_layer_stack.effective_config();
    let Some(marketplaces) = user_config
        .get("marketplaces")
        .and_then(toml::Value::as_table)
    else {
        return HashMap::new();
    };

    let default_install_root = marketplace_install_root(codex_home);
    marketplaces
        .iter()
        .filter_map(|(marketplace_name, marketplace)| {
            let marketplace_source = marketplace_sources.get(marketplace_name)?;
            let root = resolve_configured_marketplace_root(
                marketplace_name,
                marketplace,
                &default_install_root,
            )?;
            Some((root, marketplace_source.clone()))
        })
        .collect()
}

fn marketplace_config_reload(
    overrides: Vec<(String, toml::Value)>,
) -> codex_core_plugins::marketplace_upgrade::ConfigLayerReload {
    let runtime = tokio::runtime::Handle::current();
    std::sync::Arc::new(move || runtime.block_on(async {
        Config::load_with_cli_overrides(overrides.clone()).await
            .map(|config| config.config_layer_stack.as_ref().clone())
    }))
}

async fn run_upgrade(
    overrides: Vec<(String, toml::Value)>,
    args: UpgradeMarketplaceArgs,
) -> Result<()> {
    let UpgradeMarketplaceArgs {
        marketplace_name,
        json,
    } = args;
    let config = Config::load_with_cli_overrides(overrides.clone())
        .await
        .context("failed to load configuration")?;
    let codex_home = find_codex_home().context("failed to resolve CODEX_HOME")?;
    let manager = PluginsManager::new(codex_home.to_path_buf());
    let plugins_input = config.plugins_config_input();
    let reload_config = marketplace_config_reload(overrides);
    let selected_name = marketplace_name.clone();
    let outcome = tokio::task::spawn_blocking(move || manager
        .upgrade_configured_marketplaces_for_config(
            &plugins_input, selected_name.as_deref(), &reload_config,
        ))
        .await?
        .map_err(anyhow::Error::msg)?;
    if json {
        print_upgrade_outcome_json(&outcome)
    } else {
        print_upgrade_outcome(&outcome, marketplace_name.as_deref())
    }
}

async fn run_remove(
    overrides: Vec<(String, toml::Value)>,
    args: RemoveMarketplaceArgs,
) -> Result<()> {
    let RemoveMarketplaceArgs {
        marketplace_name,
        json,
    } = args;
    let codex_home = find_codex_home().context("failed to resolve CODEX_HOME")?;
    let outcome = remove_marketplace(
        codex_home.to_path_buf(),
        marketplace_config_reload(overrides),
        MarketplaceRemoveRequest { marketplace_name },
    )
    .await?;

    if json {
        let output = JsonMarketplaceRemoveOutput::from_outcome(outcome);
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    println!("Removed marketplace `{}`.", outcome.marketplace_name);
    if let Some(installed_root) = outcome.removed_installed_root {
        println!(
            "Removed installed marketplace root: {}",
            installed_root.as_path().display()
        );
    }

    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonMarketplaceRemoveOutput {
    marketplace_name: String,
    installed_root: Option<String>,
}

impl JsonMarketplaceRemoveOutput {
    fn from_outcome(outcome: MarketplaceRemoveOutcome) -> Self {
        Self {
            marketplace_name: outcome.marketplace_name,
            installed_root: outcome
                .removed_installed_root
                .map(|root| root.as_path().display().to_string()),
        }
    }
}

fn print_upgrade_outcome_json(outcome: &PluginMarketplaceUpgradeOutcome) -> Result<()> {
    for error in &outcome.errors {
        eprintln!(
            "Failed to upgrade marketplace `{}`: {}",
            error.marketplace_name, error.message
        );
    }

    let output = JsonMarketplaceUpgradeOutput::from_outcome(outcome);
    println!("{}", serde_json::to_string_pretty(&output)?);
    if !outcome.all_succeeded() {
        bail!("{} upgrade failure(s) occurred.", outcome.errors.len());
    }
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonMarketplaceUpgradeOutput {
    selected_marketplaces: Vec<String>,
    upgraded_roots: Vec<String>,
    errors: Vec<JsonMarketplaceUpgradeError>,
}

impl JsonMarketplaceUpgradeOutput {
    fn from_outcome(outcome: &PluginMarketplaceUpgradeOutcome) -> Self {
        Self {
            selected_marketplaces: outcome.selected_marketplaces.clone(),
            upgraded_roots: outcome
                .upgraded_roots
                .iter()
                .map(|root| root.display().to_string())
                .collect(),
            errors: outcome
                .errors
                .iter()
                .map(|error| JsonMarketplaceUpgradeError {
                    marketplace_name: error.marketplace_name.clone(),
                    message: error.message.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JsonMarketplaceUpgradeError {
    marketplace_name: String,
    message: String,
}

fn print_upgrade_outcome(
    outcome: &PluginMarketplaceUpgradeOutcome,
    marketplace_name: Option<&str>,
) -> Result<()> {
    for error in &outcome.errors {
        eprintln!(
            "Failed to upgrade marketplace `{}`: {}",
            error.marketplace_name, error.message
        );
    }

    let selection_label = marketplace_name.unwrap_or("all configured Git marketplaces");
    if outcome.selected_marketplaces.is_empty() {
        println!("No configured Git marketplaces to upgrade.");
    } else if outcome.upgraded_roots.is_empty() {
        if !outcome.all_succeeded() {
            println!("No marketplace upgrades completed.");
        } else if marketplace_name.is_some() {
            println!("Marketplace `{selection_label}` is already up to date.");
        } else {
            println!("All configured Git marketplaces are already up to date.");
        }
    } else if marketplace_name.is_some() {
        println!("Upgraded marketplace `{selection_label}` to the latest configured revision.");
        for root in &outcome.upgraded_roots {
            println!("Installed marketplace root: {}", root.display());
        }
    } else {
        println!("Upgraded {} marketplace(s).", outcome.upgraded_roots.len());
        for root in &outcome.upgraded_roots {
            println!("Installed marketplace root: {}", root.display());
        }
    }

    if !outcome.all_succeeded() {
        bail!("{} upgrade failure(s) occurred.", outcome.errors.len());
    }
    Ok(())
}


const OPENAI_BUNDLED_ALPHA_MARKETPLACE_NAME: &str = "openai-bundled-alpha";
const OPENAI_PRIMARY_RUNTIME_MARKETPLACE_NAME: &str = "openai-primary-runtime";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JsonMarketplaceSource {
    source_type: String,
    source: String,
}

pub(crate) fn configured_marketplace_sources(
    plugins_input: &PluginsConfigInput,
    codex_home: &Path,
) -> HashMap<String, JsonMarketplaceSource> {
    let user_config = plugins_input.config_layer_stack.effective_config();
    let Some(marketplaces) = user_config
        .get("marketplaces")
        .and_then(toml::Value::as_table)
    else {
        return HashMap::new();
    };
    let allowed_marketplace_names =
        allowed_configured_marketplace_names(&plugins_input.config_layer_stack, codex_home);

    marketplaces
        .iter()
        .filter(|(marketplace_name, _)| allowed_marketplace_names.contains(*marketplace_name))
        .filter_map(|(marketplace_name, marketplace)| {
            let source_type = marketplace
                .get("source_type")
                .and_then(toml::Value::as_str)?;
            let source = marketplace.get("source").and_then(toml::Value::as_str)?;
            Some((
                marketplace_name.clone(),
                JsonMarketplaceSource {
                    source_type: source_type.to_string(),
                    source: source.to_string(),
                },
            ))
        })
        .collect()
}

pub(crate) async fn load_cli_auth_mode(config: &Config) -> Option<AuthMode> {
    if let Some(api_key) = read_codex_api_key_from_env() {
        return Some(CodexAuth::from_api_key(&api_key).api_auth_mode());
    }

    let auth_route_config = config.auth_route_config();
    CodexAuth::from_auth_storage(
        &config.codex_home,
        config.cli_auth_credentials_store_mode,
        Some(&config.chatgpt_base_url),
        config.auth_keyring_backend_kind(),
        &auth_route_config,
    )
    .await
    .ok()
    .flatten()
    .map(|auth| auth.api_auth_mode())
}

pub(crate) struct ConfiguredMarketplaceSnapshotIssue {
    pub(crate) marketplace_name: String,
    pub(crate) path: PathBuf,
    pub(crate) message: String,
}


impl std::fmt::Display for ConfiguredMarketplaceSnapshotIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "- `{}` at {}: {}",
            self.marketplace_name,
            self.path.display(),
            self.message
        )
    }
}

pub(crate) fn configured_marketplace_snapshot_issues(
    codex_home: &std::path::Path,
    plugins_input: &PluginsConfigInput,
    load_errors: &[MarketplaceListError],
    marketplace_name: Option<&str>,
) -> Vec<ConfiguredMarketplaceSnapshotIssue> {
    let user_config = plugins_input.config_layer_stack.effective_config();
    let Some(configured_marketplaces) = user_config
        .get("marketplaces")
        .and_then(toml::Value::as_table)
    else {
        return Vec::new();
    };
    let allowed_marketplace_names =
        allowed_configured_marketplace_names(&plugins_input.config_layer_stack, codex_home);

    let default_install_root = marketplace_install_root(codex_home);
    let mut manifest_paths = Vec::new();
    let mut issues = Vec::new();
    for (configured_name, marketplace) in configured_marketplaces {
        if !allowed_marketplace_names.contains(configured_name) {
            continue;
        }
        if marketplace_name.is_some_and(|name| configured_name != name) {
            continue;
        }
        if !marketplace.is_table() {
            issues.push(ConfiguredMarketplaceSnapshotIssue {
                marketplace_name: configured_name.clone(),
                path: PathBuf::from("<invalid config>"),
                message: "configured marketplace entry must be a table".to_string(),
            });
            continue;
        }
        if let Err(err) = validate_plugin_segment(configured_name, "marketplace name") {
            issues.push(ConfiguredMarketplaceSnapshotIssue {
                marketplace_name: configured_name.clone(),
                path: PathBuf::from("<invalid config>"),
                message: err.to_string(),
            });
            continue;
        }
        if marketplace.get("source_type").and_then(toml::Value::as_str) == Some("local")
            && marketplace
                .get("source")
                .and_then(toml::Value::as_str)
                .is_none_or(str::is_empty)
        {
            issues.push(ConfiguredMarketplaceSnapshotIssue {
                marketplace_name: configured_name.clone(),
                path: PathBuf::from("<invalid source>"),
                message: "configured local marketplace source is missing or empty".to_string(),
            });
            continue;
        }
        let Some(root) = resolve_configured_marketplace_root(
            configured_name,
            marketplace,
            &default_install_root,
        ) else {
            continue;
        };
        match find_marketplace_manifest_path(&root) {
            Some(path) => manifest_paths.push((configured_name.clone(), path)),
            None => {
                if is_implicit_system_marketplace_root(configured_name, &root) {
                    continue;
                }
                issues.push(ConfiguredMarketplaceSnapshotIssue {
                    marketplace_name: configured_name.clone(),
                    path: root,
                    message: "marketplace root does not contain a supported manifest".to_string(),
                });
            }
        }
    }

    for error in load_errors {
        if let Some((configured_name, _)) = manifest_paths
            .iter()
            .find(|(_, path)| path.as_path() == error.path.as_path())
        {
            issues.push(ConfiguredMarketplaceSnapshotIssue {
                marketplace_name: configured_name.clone(),
                path: error.path.to_path_buf(),
                message: error.message.clone(),
            });
        }
    }
    issues
}

fn is_implicit_system_marketplace_root(
    marketplace_name: &str,
    root: &Path,
) -> bool {
    if matches!(
        marketplace_name,
        OPENAI_BUNDLED_MARKETPLACE_NAME | OPENAI_BUNDLED_ALPHA_MARKETPLACE_NAME
    ) && path_ends_with(root, &[".tmp", "bundled-marketplaces", marketplace_name])
    {
        return true;
    }

    marketplace_name == OPENAI_PRIMARY_RUNTIME_MARKETPLACE_NAME
        && path_ends_with(
            root,
            &[
                "codex-runtimes",
                "codex-primary-runtime",
                "plugins",
                marketplace_name,
            ],
        )
}

fn path_ends_with(path: &Path, suffix: &[&str]) -> bool {
    let path_components = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    path_components.as_slice().ends_with(
        &suffix
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>(),
    )
}
