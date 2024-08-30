use std::collections::{hash_map, HashMap};
use std::fmt::{Display, Formatter};
use std::process::ExitCode;
use std::time::Duration;
use reqwest::{Client, Error, StatusCode};
use serde::{Serialize};
use serde_json::Value;
use tracing::{debug, error, info, Level};
use std::env;
use std::str::FromStr;

#[derive(Clone, Debug)]
struct Config {
    qb_address: String,
    qb_username: String,
    qb_password: String,
    jellyfin_address: String,
    jellyfin_api_token: String,
    jellyfin_active_within_secs: u64,
    poll_time_secs: u64,
}

#[derive(Serialize, Clone, Debug)]
struct QBCreds {
    username: String,
    password: String
}

impl From<&Config> for QBCreds {
    fn from(value: &Config) -> Self {
        QBCreds {
            username: value.qb_username.clone(),
            password: value.qb_password.clone()
        }
    }
}

impl Display for QBCreds {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "username={}&password={}", self.username, self.password)
    }
}

enum ThrottlerError {
    ReqwestError(String),
    BadResponse(String, StatusCode),
    NoCookie,
}

impl Display for ThrottlerError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let display_str = match self {
            ThrottlerError::ReqwestError(message) => {message.as_str()}
            ThrottlerError::BadResponse(message, _status) => {message.as_str()}
            ThrottlerError::NoCookie => {"No Cookie Returned"}
        };

        write!(f, "{}", display_str)
    }
}

impl From<Error> for ThrottlerError {
    fn from(value: Error) -> Self {
        ThrottlerError::ReqwestError(format!("Error calling QBittorrent. Status: {}", value))
    }
}

fn apply_env<I>(current_env: &mut HashMap<String, Option<String>>, load_env: I) where I: Iterator<Item=(String, String)> {
    for env_var in load_env {
        if let hash_map::Entry::Occupied(mut e) = current_env.entry(env_var.0) {
            e.insert(Some(env_var.1.to_string()));
        }
    }
}

const DEFAULT_POLL_TIME_SECS: u64 = 5;
const DEFAULT_JELLYFIN_ACTIVE_WITHIN_SECS: u64 = 5;

#[tokio::main]
async fn main() -> ExitCode {
    let collector = tracing_subscriber::fmt()
        .with_max_level(get_log_level())
        .finish();
    tracing::subscriber::set_global_default(collector).unwrap();

    let config = match load_config() {
        Ok(config) => {config}
        Err(err) => {return err}
    };

    info!("Starting up");
    let client = Client::new();

    loop {
        let cookie_req = qb_auth(&client, &config).await;

        let cookie = match cookie_req {
            Ok(cookie) => { cookie }
            Err(err) => {
                match err {
                    ThrottlerError::BadResponse(_, code) => {
                        if code == StatusCode::UNAUTHORIZED || code == StatusCode::FORBIDDEN {
                            error!("qBittorrent Auth failed critically. Check credentials");
                            break;
                        }
                    }
                    ThrottlerError::NoCookie => {
                        error!("qBittorrent Auth failed critically. Check credentials");
                        break;
                    },
                    _ => {
                        info!("Auth failure not critical, retrying in {} seconds", config.poll_time_secs)
                    }
                }

                //Any errors that aren't auth related should be solved by waiting
                continue;
            }
        };
        debug!("{}", cookie);

        loop {
            let sessions_req = jellyfin_get_sessions(&client, &config).await;
            let sessions = match sessions_req {
                Ok(sessions) => { sessions }
                Err(err) => {
                    error!("{err}");
                    return 1.into();
                }
            };

            let speed = if sessions > 0 {
                debug!("Session is active, throttling");
                1000
            } else {
                debug!("Session is not active, removing throttling");
                0
            };
            if let Err(ThrottlerError::BadResponse(_, status)) = qb_set_upload(&client, &config, &cookie, speed).await {
                //Exit the loop to re-auth if auth fails
                if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
                    break;
                }
            }

            tokio::time::sleep(Duration::from_secs(config.poll_time_secs)).await;
        }
    }

    0.into()
}

fn get_log_level() -> Level {
    let mut log_level = Level::INFO;
    let env_log_level = env::var("QB_THROTTLER_LOG_LEVEL");
    let dot_env_log_level = dotenv::var("QB_THROTTLER_LOG_LEVEL");

    if env_log_level.is_ok() {
        log_level = Level::from_str(&env_log_level.unwrap()).unwrap_or(log_level);
    }

    if dot_env_log_level.is_ok() {
        log_level = Level::from_str(&dot_env_log_level.unwrap()).unwrap_or(log_level);
    }

    log_level
}

fn load_config() -> Result<Config, ExitCode> {
    let env_vars = env::vars();
    let dot_env_vars = dotenv::vars();

    //Start with defaults
    let mut env_config: HashMap<String, Option<String>> = HashMap::from([
        ("QB_ADDRESS".to_string(), None),
        ("QB_USERNAME".to_string(), None),
        ("QB_PASSWORD".to_string(), None),
        ("JELLYFIN_ADDR".to_string(), None),
        ("JELLYFIN_TOKEN".to_string(), None),
        ("JELLYFIN_ACTIVE_WITHIN_SECS".to_string(), Some("5".to_string())),
        ("QB_THROTTLER_POLL_FREQ".to_string(), Some("5".to_string()))
    ]);

    apply_env(&mut env_config, env_vars);

    //Dotenv is more specific so we override system env with it
    apply_env(&mut env_config, dot_env_vars);

    if env_config.iter().any(|x| x.1.is_none()) {
        for entry in env_config.iter().filter(|x| x.1.is_none()) {
            error!("Config is missing missing for env variable: {}", entry.0);
        }
        return Err(1.into());
    }

    Ok(Config {
        qb_address: env_config["QB_ADDRESS"].as_ref().unwrap().to_string(),
        qb_username: env_config["QB_USERNAME"].as_ref().unwrap().to_string(),
        qb_password: env_config["QB_PASSWORD"].as_ref().unwrap().to_string(),
        jellyfin_address: env_config["JELLYFIN_ADDR"].as_ref().unwrap().to_string(),
        jellyfin_api_token: env_config["JELLYFIN_TOKEN"].as_ref().unwrap().to_string(),
        jellyfin_active_within_secs: env_config["JELLYFIN_ACTIVE_WITHIN_SECS"].as_ref().unwrap().trim().parse().unwrap_or_else(|_| {
            error!("JELLYFIN_ACTIVE_WITHIN_SECS env var was not a valid integer. Defaulting to {DEFAULT_JELLYFIN_ACTIVE_WITHIN_SECS}");
            DEFAULT_JELLYFIN_ACTIVE_WITHIN_SECS
        }),
        poll_time_secs: env_config["QB_THROTTLER_POLL_FREQ"].as_ref().unwrap().trim().parse().unwrap_or_else(|_| {
            error!("JELLYFIN_ACTIVE_WITHIN_SECS env var was not a valid integer. Defaulting to {DEFAULT_POLL_TIME_SECS}");
            DEFAULT_POLL_TIME_SECS
        })
    })
}

async fn jellyfin_get_sessions(client: &Client, config: &Config) -> Result<usize, ThrottlerError> {
    let response = client
        .get(format!("{}/Sessions?activeWithinSeconds={}", &config.jellyfin_address, config.jellyfin_active_within_secs))
        .header("Authorization", format!("MediaBrowser Token={}", &config.jellyfin_api_token))
        .send()
        .await?.json::<Value>().await?;
    debug!("{:?}", response);

    //Don't care about session details, we only care if any are active
    if let Some(session_list) = response.as_array() {
        Ok(session_list.len())
    } else {
        Ok(0)
    }
}

async fn qb_auth(client: &Client, config: &Config) -> Result<String, ThrottlerError> {
    let response = client.post(format!("{}/api/v2/auth/login", &config.qb_address))
        .header("Referer", &config.qb_address)
        .form(&QBCreds::from(config))
        .send()
        .await?;

    let status = response.status();
    if status != StatusCode::OK {
        return Err(ThrottlerError::BadResponse(format!("Bad Response from QBittorrent: {status}"), status));
    }

    debug!("Reponse headers: {:?}", response.headers());

    let cookie = response.headers().get("set-cookie");

    match cookie {
        Some(token) => {
            match token.to_str() {
                Ok(token_str) => {
                    Ok(token_str.to_string())
                }
                Err(_) => {
                    Err(ThrottlerError::NoCookie)
                }
            }
        }
        None => {
            Err(ThrottlerError::NoCookie)
        }
    }
}

async fn qb_set_upload(client: &Client, config: &Config, cookie: &String, speed: u32) -> Result<(), ThrottlerError> {
    let mut payload = HashMap::new();
    payload.insert("limit", speed);
    let response = client.post(format!("{}/api/v2/transfer/setUploadLimit", &config.qb_address))
        .header("Cookie", cookie)
        .form(&payload)
        .send()
        .await?;
    debug!("{response:?}");
    
    let status = response.status();
    if status != StatusCode::OK {
        return Err(ThrottlerError::BadResponse(format!("Bad Response from QBittorrent: {status}"), status));
    }
    
    Ok(())
}