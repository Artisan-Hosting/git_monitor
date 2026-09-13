use artisan_middleware::aggregator::{Metrics, Status};
use artisan_middleware::config::AppConfig;
use artisan_middleware::dusa_collection_utils::core::logger::{set_log_level, LogLevel};
use artisan_middleware::dusa_collection_utils::core::types::pathtype::PathType;
use artisan_middleware::dusa_collection_utils::core::types::stringy::Stringy;
use artisan_middleware::dusa_collection_utils::core::version::{
    SoftwareVersion, Version, VersionCode,
};
use artisan_middleware::dusa_collection_utils::log;
use artisan_middleware::resource_monitor::ResourceMonitorLock;
use artisan_middleware::state_persistence::{self, update_state, AppState, StatePersistence};
use artisan_middleware::timestamp::current_timestamp;
use artisan_middleware::version::{aml_version, str_to_version};
use std::fs;

const DEFAULT_APP_CONFIG_DIR: &str = "/etc/ais_gitmon";
const ERROR_LOG_MAX_SIZE: usize = 5;

/// Caps `state.error_log` in place. The library's own `log_error()` pushes
/// to this vec via a raw, uncapped `update_state()`, so a run of consecutive
/// failures across many repo workers can otherwise grow it unbounded between
/// [`update_state_wrapper`] heartbeats.
pub fn truncate_error_log(state: &mut AppState) {
    if state.error_log.len() > ERROR_LOG_MAX_SIZE {
        state.data = format!(
            "The error log has a legnth of {}. Truncating...",
            state.error_log.len()
        );
        state.error_log.truncate(ERROR_LOG_MAX_SIZE);
    }
}

/// Root directory for this app's git config/state. Overridable via
/// `AIS_GITMON_CONFIG_DIR` so tests can run against a hermetic temp
/// directory instead of the real system path.
pub fn app_config_dir() -> String {
    std::env::var("AIS_GITMON_CONFIG_DIR").unwrap_or_else(|_| DEFAULT_APP_CONFIG_DIR.to_string())
}

/// Path to the git global-config file this app manages (safe.directory
/// entries, etc.). See [`app_config_dir`] for the override mechanism.
pub fn app_git_config_path() -> String {
    format!("{}/gitconfig", app_config_dir())
}

pub fn get_config() -> AppConfig {
    if let Ok(content) = fs::read_to_string("runtime.toml") {
        if let Ok(env_v2) = toml::from_str::<artisan_middleware::enviornment::definitions::Enviornment_V2>(&content) {
            log!(LogLevel::Info, "Loaded gitmon configuration from Environment V2 (runtime.toml)");
            return AppConfig {
                app_name: env_v2.app_name,
                max_ram_usage: env_v2.max_ram_usage,
                max_cpu_usage: env_v2.max_cpu_usage,
                environment: env_v2.environment.to_string(),
                debug_mode: env_v2.debug_mode,
                log_level: env_v2.log_level,
                git: env_v2.git,
                database: None,
                aggregator: env_v2.aggregator,
            };
        }
    }

    let mut config: AppConfig = match AppConfig::new() {
        Ok(loaded_data) => loaded_data,
        Err(e) => {
            log!(LogLevel::Error, "Couldn't load config: {}", e.to_string());
            std::process::exit(0)
        }
    };
    config.app_name = Stringy::from(env!("CARGO_PKG_NAME"));
    config.database = None;
    config
}

pub async fn generate_state(config: &AppConfig) -> AppState {
    let state_path: PathType = get_state_path(&config);

    match StatePersistence::load_state(&state_path).await {
        Ok(mut loaded_data) => {
            log!(LogLevel::Info, "Loaded previous state data");
            // log!(LogLevel::Trace, "Previous state data: {:#?}", loaded_data);
            loaded_data.data = String::from("Initializing");
            loaded_data.config.debug_mode = config.debug_mode;
            loaded_data.version = {
                let library_version: Version = aml_version();
                let software_version: Version =
                    str_to_version(env!("CARGO_PKG_VERSION"), Some(VersionCode::Production));

                SoftwareVersion {
                    application: software_version,
                    library: library_version,
                }
            };
            loaded_data.config.git = config.git.clone();
            loaded_data.last_updated = current_timestamp();
            loaded_data.config.log_level = config.log_level;
            loaded_data.config.aggregator = config.aggregator.clone();
            loaded_data.config.environment = config.environment.clone();
            loaded_data.stared_at = current_timestamp();
            loaded_data.pid = std::process::id();
            loaded_data.error_log.clear();
            set_log_level(loaded_data.config.log_level);
            loaded_data.event_counter = 0;
            if config.debug_mode == true {
                set_log_level(LogLevel::Debug);
            }
            loaded_data.error_log.clear();
            update_state(&mut loaded_data, &state_path, None).await;
            loaded_data
        }
        Err(e) => {
            log!(LogLevel::Warn, "No previous state loaded, creating new one");
            log!(LogLevel::Debug, "Error loading previous state: {}", e);
            let mut state = AppState {
                name: env!("CARGO_PKG_NAME").to_owned(),
                version: SoftwareVersion::dummy(),
                data: String::new(),
                last_updated: current_timestamp(),
                stared_at: current_timestamp(),
                event_counter: 0,
                pid: std::process::id(),
                error_log: vec![],
                config: config.clone(),
                system_application: true,
                status: Status::Starting,
                stdout: Vec::new(),
                stderr: Vec::new(),
            };
            state.data = String::from("Initializing");
            state.config.debug_mode = true;
            state.last_updated = current_timestamp();
            state.config.log_level = config.log_level;
            state.config.environment = config.environment.clone();
            state.version = {
                let library_version: Version = aml_version();
                let software_version: Version =
                    str_to_version(env!("CARGO_PKG_VERSION"), Some(VersionCode::Production));

                SoftwareVersion {
                    application: software_version,
                    library: library_version,
                }
            };
            if config.debug_mode == true {
                set_log_level(LogLevel::Debug);
            }
            state.error_log.clear();
            update_state(&mut state, &state_path, None).await;
            state
        }
    }
}

pub fn get_state_path(config: &AppConfig) -> PathType {
    state_persistence::StatePersistence::get_state_path(&config)
}

pub fn get_git_token_file() -> Option<String> {
    if let Ok(contents) = fs::read_to_string("runtime.toml") {
        if let Some(path) = parse_token_file_path(&contents) {
            return Some(path);
        }
    }

    let contents = match fs::read_to_string("Overrides.toml") {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            log!(
                LogLevel::Warn,
                "Failed to read Overrides.toml while locating the GitHub token file: {}",
                err
            );
            return None;
        }
    };

    parse_token_file_path(&contents)
}

// Split out from `get_git_token_file` so the parsing logic can be
// regression-tested without touching the process's CWD or the filesystem.
fn parse_token_file_path(contents: &str) -> Option<String> {
    // `contents.parse::<toml::Value>()` uses `toml::Value`'s own `FromStr`,
    // which only accepts a single bare value (a string, a number, an inline
    // table, ...), not a full multi-line document -- a real Overrides.toml
    // (which starts with a `#` comment) fails to parse there and this
    // function would incorrectly report no token_file configured, silently
    // falling back to `gh auth token`. `toml::from_str` parses a whole
    // document, which is what Overrides.toml actually is. See the same
    // footgun documented in `auth::parse_token`.
    let parsed = match toml::from_str::<toml::Value>(contents) {
        Ok(parsed) => parsed,
        Err(err) => {
            log!(
                LogLevel::Warn,
                "Failed to parse Overrides.toml while locating the GitHub token file: {}",
                err
            );
            return None;
        }
    };

    parsed
        .get("git")
        .and_then(|git| git.get("token_file"))
        .and_then(toml::Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod token_file_path_tests {
    use super::parse_token_file_path;

    #[test]
    fn parses_token_file_with_leading_comment() {
        let contents = "# Overrides for the default config from the lib\n\n\
             debug_mode = true\n\
             log_level = \"Info\"\n\
             \n\
             [git]\n\
             default_server = \"GitHub\"\n\
             credentials_file = \"/tmp/git.recs\"\n\
             token_file = \"/tmp/github.token\"\n";

        assert_eq!(
            parse_token_file_path(contents),
            Some("/tmp/github.token".to_string())
        );
    }

    #[test]
    fn returns_none_when_git_section_missing_token_file() {
        let contents = "[git]\ndefault_server = \"GitHub\"\n";
        assert_eq!(parse_token_file_path(contents), None);
    }

    #[test]
    fn returns_none_on_malformed_toml() {
        let contents = "# comment\n[git\ntoken_file = \"/tmp/github.token\"\n";
        assert_eq!(parse_token_file_path(contents), None);
    }
}

pub async fn update_state_wrapper(
    state: &mut AppState,
    path: &PathType,
    monitor: &Option<ResourceMonitorLock>,
) {
    let mut metrics: Option<Metrics> = None;

    if let Some(monitor) = monitor {
        match monitor.get_metrics().await {
            Ok(met) => metrics = Some(met),
            Err(err) => {
                log!(
                    LogLevel::Error,
                    "Failed to get monitor data: {}",
                    err.err_mesg
                );
            }
        }
    }

    truncate_error_log(state);

    update_state(state, &path, metrics).await;
}
