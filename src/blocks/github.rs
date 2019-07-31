extern crate lazy_static;
extern crate regex;
extern crate reqwest;

use crossbeam_channel::Sender;
use std::collections::HashMap;
use std::time::Duration;

use crate::block::{Block, ConfigBlock};
use crate::config::Config;
use crate::de::deserialize_duration;
use crate::errors::*;
use crate::input::I3BarEvent;
use crate::regex::Regex;
use crate::scheduler::Task;
use crate::util::FormatTemplate;
use crate::widget::I3BarWidget;
use crate::widgets::text::TextWidget;
use uuid::Uuid;

const GITHUB_TOKEN_ENV: &str = "GITHUB_TOKEN";

pub struct Github {
    text: TextWidget,
    id: String,
    update_interval: Duration,
    gh: GithubClient,
    format: FormatTemplate,
}

#[derive(Deserialize, Debug, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    /// Update interval in seconds
    #[serde(
        default = "GithubConfig::default_interval",
        deserialize_with = "deserialize_duration"
    )]
    pub interval: Duration,

    #[serde(default = "GithubConfig::default_api_server")]
    pub api_server: String,

    /// Format override
    #[serde(default = "GithubConfig::default_format")]
    pub format: String,
}

impl GithubConfig {
    fn default_interval() -> Duration {
        Duration::from_secs(30)
    }

    fn default_api_server() -> String {
        "https://api.github.com".to_owned()
    }

    fn default_format() -> String {
        "{total}".to_owned()
    }
}

impl ConfigBlock for Github {
    type Config = GithubConfig;

    fn new(block_config: Self::Config, config: Config, _: Sender<Task>) -> Result<Self> {
        let token = match std::env::var(GITHUB_TOKEN_ENV).ok() {
            Some(v) => v,
            None => {
                return Err(BlockError(
                    "github".to_owned(),
                    "missing GITHUB_TOKEN environment variable".to_owned(),
                ))
            }
        };

        Ok(Github {
            id: Uuid::new_v4().simple().to_string(),
            update_interval: block_config.interval,
            text: TextWidget::new(config.clone()).with_text("N/A").with_icon("github"),
            gh: GithubClient::new(block_config.api_server, token),
            format: FormatTemplate::from_string(&block_config.format)
                .block_error("github", "Invalid format specified")?,
        })
    }
}

impl Block for Github {
    fn update(&mut self) -> Result<Option<Duration>> {
        let aggregations = match self.gh.notifications().try_fold(
            map!("total".to_owned() => 0),
            |mut acc, notif| -> std::result::Result<HashMap<String, u64>, Box<dyn std::error::Error>> {
                let n = notif?;
                acc.entry(n.reason).and_modify(|v| *v += 1).or_insert(1);
                acc.entry("total".to_owned()).and_modify(|v| *v += 1);
                Ok(acc)
            },
        ) {
            Ok(v) => v,
            Err(_) => {
                // If there is a error reported, set the value to N/A
                self.text.set_text("N/A".to_owned());
                return Ok(Some(self.update_interval));
            }
        };

        let default: u64 = 0;
        let values = map!(
            "{total}" => format!("{}", aggregations.get("total").unwrap_or(&default)),
            // As specified by:
            // https://developer.github.com/v3/activity/notifications/#notification-reasons
            "{assign}" => format!("{}", aggregations.get("assign").unwrap_or(&default)),
            "{author}" => format!("{}", aggregations.get("author").unwrap_or(&default)),
            "{comment}" => format!("{}", aggregations.get("comment").unwrap_or(&default)),
            "{invitation}" => format!("{}", aggregations.get("invitation").unwrap_or(&default)),
            "{manual}" => format!("{}", aggregations.get("manual").unwrap_or(&default)),
            "{mention}" => format!("{}", aggregations.get("mention").unwrap_or(&default)),
            "{review_requested}" => format!("{}", aggregations.get("review_requested").unwrap_or(&default)),
            "{security_alert}" => format!("{}", aggregations.get("security_alert").unwrap_or(&default)),
            "{state_change}" => format!("{}", aggregations.get("state_change").unwrap_or(&default)),
            "{subscribed}" => format!("{}", aggregations.get("subscribed").unwrap_or(&default)),
            "{team_mention}" => format!("{}", aggregations.get("team_mention").unwrap_or(&default))
        );

        self.text.set_text(self.format.render_static_str(&values)?);

        Ok(Some(self.update_interval))
    }

    fn view(&self) -> Vec<&I3BarWidget> {
        vec![&self.text]
    }

    fn click(&mut self, _: &I3BarEvent) -> Result<()> {
        Ok(())
    }

    fn id(&self) -> &str {
        &self.id
    }
}

struct GithubClient {
    http: reqwest::Client,
    api_server: String,
    token: String,
}

impl GithubClient {
    fn new(api_server: String, token: String) -> Self {
        GithubClient {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            api_server: api_server,
            token: token,
        }
    }

    fn notifications(&self) -> Notifications {
        Notifications {
            http: &self.http,
            next_page_url: format!("{}/notifications", self.api_server),
            token: &self.token,
            notifications: vec![].into_iter(),
        }
    }
}

struct Notifications<'a> {
    notifications: <Vec<Notification> as IntoIterator>::IntoIter,
    http: &'a reqwest::Client,
    token: &'a str,
    next_page_url: String,
}

impl<'a> Iterator for Notifications<'a> {
    type Item = std::result::Result<Notification, Box<dyn std::error::Error>>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.try_next() {
            Ok(Some(notif)) => Some(Ok(notif)),
            Ok(None) => None,
            Err(err) => Some(Err(err)),
        }
    }
}

impl<'a> Notifications<'a> {
    fn try_next(&mut self) -> std::result::Result<Option<Notification>, Box<dyn std::error::Error>> {
        if let Some(notif) = self.notifications.next() {
            return Ok(Some(notif));
        }

        if self.next_page_url == "" {
            return Ok(None);
        }

        let mut response = self
            .http
            .get(&self.next_page_url)
            .header(reqwest::header::AUTHORIZATION, format!("token {}", self.token))
            .send()?;

        if !response.status().is_success() {
            return Err(Box::new(response.json::<GithubError>()?));
        }

        let link_header = match response.headers().get(reqwest::header::LINK) {
            Some(v) => v.to_str()?,
            None => "",
        };

        self.next_page_url = match extract_links(link_header).get(&"next") {
            Some(url) => url.to_string(),
            None => "".to_owned(),
        };

        self.notifications = response.json::<Vec<Notification>>()?.into_iter();

        Ok(self.notifications.next())
    }
}

#[derive(Deserialize)]
struct Notification {
    reason: String,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubError {
    message: String,
}

impl std::fmt::Display for GithubError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for GithubError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        None
    }
}

fn extract_links(raw_links: &str) -> HashMap<&str, &str> {
    lazy_static! {
        static ref LINKS_REGEX: Regex =
            Regex::new(r#"(<(?P<url>http(s)?://[^>\s]+)>; rel="(?P<rel>[[:word:]]+))+"#).unwrap();
    }

    LINKS_REGEX
        .captures_iter(raw_links)
        .fold(HashMap::new(), |mut acc, cap| {
            let groups = (cap.name("url"), cap.name("rel"));
            match groups {
                (Some(url), Some(rel)) => {
                    acc.insert(rel.as_str(), url.as_str());
                    acc
                }
                _ => acc,
            }
        })
}
