use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::process::ExitCode;
use std::time::Duration;
use reqwest::{Client, Error, StatusCode};
use serde::{Serialize};
use serde_json::Value;
use tracing::{debug, error, Level};

#[derive(Clone, Debug)]
struct Config {
    log_level: Level,
    qb_address: String,
    qb_username: String,
    qb_password: String,
    jellyfin_address: String,
    jellyfin_api_token: String,
    jellyfin_active_within_secs: u32,
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

#[tokio::main]
async fn main() -> ExitCode {
    let config = Config {
        log_level: Level::DEBUG,
        qb_address: "https://qbittorrent.ryancarins.info".to_string(),
        qb_username: "".to_string(),
        qb_password: "".to_string(),
        jellyfin_address: "http://192.168.1.12:8096".to_string(),
        jellyfin_api_token: "".to_string(),
        jellyfin_active_within_secs: 60
    };

    let collector = tracing_subscriber::fmt()
        // filter spans/events with level TRACE or higher.
        .with_max_level(config.log_level)
        // build but do not install the subscriber.
        .finish();

    tracing::subscriber::set_global_default(collector).unwrap();

    debug!("Starting up");
    let client = Client::new();


    loop {
        let cookie_req = qb_auth(&client, &config).await;

        let cookie = match cookie_req {
            Ok(cookie) => { cookie }
            Err(err) => {
                error!("{err}");
                if let ThrottlerError::BadResponse(_, code) = err {
                    //Exit the loop to re-auth if auth fails
                    if code == StatusCode::UNAUTHORIZED || code == StatusCode::FORBIDDEN {
                        break;
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

            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    0.into()
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