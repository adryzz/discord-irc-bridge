use irc::proto::Command;
use log::error;
use log::info;
use log::trace;
use log::Level;
use serde::Deserialize;
use serenity::async_trait;
use serenity::framework::StandardFramework;
use serenity::model::id::GuildId;
use serenity::model::prelude::ChannelId;
use serenity::model::prelude::ChannelType;
use serenity::model::prelude::Ready;
use serenity::{futures::StreamExt, http::Http, model::webhook::Webhook};
use std::collections::HashMap;
use std::sync::Arc;
type IrcClient = irc::client::Client;
use anyhow::Result;
use serenity::prelude::*;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().init();
    info!("Starting up Discord <-> IRC bridge...");

    if let Err(a) = run().await {
        error!("{}", a);
    }
    info!("Shutting down Discord <-> IRC bridge...");
}

async fn run() -> Result<()> {
    let config = try_read_config("config.toml").await?;

    let intents = GatewayIntents::GUILD_WEBHOOKS;

    let framework = StandardFramework::new();

    let handler = Handler {
        config: config.clone(),
    };

    let mut discord = Client::builder(&config.token, intents)
        .framework(framework)
        .event_handler(handler)
        .await?;

    discord.start().await?;

    Ok(())
}

async fn listen_irc(http: Arc<Http>, guild_id: u64) -> Result<()> {
    let guild = GuildId(guild_id);

    let bridged_channels = get_bridged_channels(&http, &guild).await?;

    let bridge_webhooks = get_or_create_webhooks(&http, &bridged_channels).await?;

    let mut client = IrcClient::new("irc-config.toml").await?;
    client.identify()?;

    let mut stream = client.stream()?;

    while let Some(message) = stream.next().await.transpose()? {
        match &message.command {
            Command::PRIVMSG(channel, text) => {
                let hook = get_correct_webhook(channel, &bridge_webhooks).await?;
                if let Some(h) = hook {
                    let name = message.source_nickname().unwrap_or("null");
                    h.execute(&http, false, |m| {
                        m.username(name)
                            .content(text)
                            .avatar_url(format!("https://singlecolorimage.com/get/{:06x}/1x1", get_color_from_name(name)))
                    })
                    .await?;
                }
            }
            Command::TOPIC(channel, text) => {
                if let Some(chan) = get_correct_channel(channel, &bridged_channels).await? {
                    chan.edit(&http, |f| f.topic(text.as_ref().map_or("", |x| x.as_str())))
                        .await?;
                }
            }
            _ => (),
        }
    }

    Ok(())
}

fn get_color_from_name(name: &str) -> u32 {
    crc32fast::hash(name.as_bytes()) >> 8
}

async fn get_correct_webhook<'a>(
    channel: &str,
    bridge_webhooks: &'a HashMap<String, Webhook>,
) -> Result<Option<&'a Webhook>> {
    for b in bridge_webhooks {
        let name = &channel[1..];

        if b.0 == name {
            return Ok(Some(b.1));
        }
    }

    Ok(None)
}

async fn get_correct_channel<'a>(
    channel: &str,
    bridge_webhooks: &'a HashMap<ChannelId, String>,
) -> Result<Option<&'a ChannelId>> {
    for b in bridge_webhooks {
        let name = &channel[1..];
        info!("{}, {}", name, b.1);

        if b.1 == name {
            return Ok(Some(b.0));
        }
    }

    Ok(None)
}

async fn get_bridged_channels(
    http: &Arc<Http>,
    guild: &GuildId,
) -> Result<HashMap<ChannelId, String>> {
    let channels = guild.channels(http).await?;

    let irc_category = channels
        .iter()
        .filter(|x| x.1.kind == ChannelType::Category)
        .filter(|x| x.1.name() == "irc")
        .last();

    let mut vec = HashMap::new();

    if let Some(category) = irc_category {
        vec = channels
            .iter()
            .filter(|x| x.1.parent_id.is_some())
            .filter(|x| x.1.parent_id.unwrap().0 == category.0 .0)
            .map(|x| (*x.0, x.1.name.clone()))
            .collect();
    }

    Ok(vec)
}

async fn get_or_create_webhooks(
    http: &Arc<Http>,
    channels: &HashMap<ChannelId, String>,
) -> Result<HashMap<String, Webhook>> {
    let mut list: HashMap<String, Webhook> = HashMap::with_capacity(channels.len());

    'outer: for c in channels {
        for hook in c.0.webhooks(http).await? {
            if let Some(name) = &hook.name {
                if name.as_str() == "irc" {
                    info!("Found existing webhook for channel {} ({}).", &c.1, c.0 .0);
                    list.insert(c.1.clone(), hook);
                    continue 'outer;
                }
            }
        }
        let hook = c.0.create_webhook(http, "irc").await?;
        info!("Created webhook for channel {} ({}).", &c.1, c.0 .0);
        list.insert(c.1.clone(), hook);
    }

    Ok(list)
}

struct Handler {
    config: Config,
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("Discord connection ready");
        info!("Starting IRC connection...");
        tokio::spawn(listen_irc(ctx.http, self.config.guild_id));
    }
}

#[derive(Debug, Deserialize, Clone)]
struct Config {
    token: String,
    guild_id: u64,
}

async fn try_read_config(file: &str) -> Result<Config> {
    let mut file = File::open(file).await?;
    let mut data = String::new();
    file.read_to_string(&mut data).await?;

    let a: Config = toml::from_str(data.as_str())?;

    Ok(a)
}
