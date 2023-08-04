use irc::proto::Command;

use poise::serenity_prelude as serenity;
use serenity::Interaction;
use serenity::Ready;
use serenity::UserId;
use serde::Deserialize;
use serenity::model::id::GuildId;
use serenity::model::prelude::ChannelId;
use serenity::model::prelude::ChannelType;
use serenity::{futures::StreamExt, http::Http, model::webhook::Webhook};
use tokio::sync::RwLock;
use tokio::sync::broadcast;
use tracing::debug;
use tracing::error;
use tracing::info;
use std::collections::HashMap;
use std::sync::Arc;
type IrcClient = irc::client::Client;
use anyhow::Result;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
type Context<'a> = poise::Context<'a, Data, anyhow::Error>;

#[tokio::main]
async fn main() {
    console_subscriber::init();
    info!("Starting up Discord <-> IRC bridge...");

    if let Err(a) = run().await {
        error!("{}", a);
    }
    info!("Shutting down Discord <-> IRC bridge...");
}

async fn run() -> Result<()> {
    let config = try_read_config("config.toml").await?;

    let intents = serenity::GatewayIntents::GUILD_WEBHOOKS;

    let (tx, _rx) = broadcast::channel(64);

    let mut handler = Handler {
        options: poise::FrameworkOptions {
            commands: vec![write()],
            ..Default::default()
        },
        data: Data {
            config: config.clone(),
            tx,
        },
        shard_manager: std::sync::Mutex::new(None),
        bot_id: RwLock::new(None)
    };

    poise::set_qualified_names(&mut handler.options.commands);

    let handler = std::sync::Arc::new(handler);

    let mut client = serenity::Client::builder(config.token, intents)
        .event_handler_arc(handler.clone())
        .await?;

    *handler.shard_manager.lock().unwrap() = Some(client.shard_manager.clone());
    client.start().await?;

    Ok(())
}

async fn listen_irc(http: Arc<Http>, guild_id: u64, mut rx: broadcast::Receiver<CMessage>) -> Result<()> {
    let guild = GuildId(guild_id);

    let bridged_channels = get_bridged_channels(&http, &guild).await?;

    let bridge_webhooks = get_or_create_webhooks(&http, &bridged_channels).await?;

    let mut client = IrcClient::new("irc-config.toml").await?;
    client.identify()?;

    let mut stream = client.stream()?;
    let sender = client.sender();

    loop {
        tokio::select! {
            s = stream.next() => {
                if let Some(message) = s.transpose()? {
                    match &message.command {
                        Command::PRIVMSG(channel, text) => {
                            let hook = get_correct_webhook(&channel, &bridge_webhooks).await?;
                            if let Some(h) = hook {
                                let name = message.source_nickname().unwrap_or("null");
                                h.execute(&http, false, |m| {
                                    m.username(name)
                                        .content(text)
                                        .avatar_url(format!("https://singlecolorimage.com/get/{:06x}/1x1", get_color_from_name(name)))
                                })
                                .await?;
                            debug!("message received in {}: {}", channel, text);
                            }
                        }
                        Command::TOPIC(channel, text) => {
                            if let Some(chan) = get_correct_channel(&channel, &bridged_channels).await? {
                                chan.edit(&http, |f| f.topic(text.as_ref().map_or("", |x| x.as_str())))
                                    .await?;
                            }
                        }
                        _ => (),
                    }
                }

            }
    
            Ok(msg) = rx.recv() => {
                if let Some(c) = bridged_channels.get(&msg.channel) {
                    debug!("sending \"{}\" in #{}", &msg.message, &c);
                    sender.send_privmsg(format!("#{}", &c), &msg.message)?;
                } else {
                    // channel not bridged
                }
            }
        }
    }
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

#[poise::command(slash_command)]
async fn write(
    ctx: Context<'_>,
    #[description = "Message"] msg: String
) -> Result<(), anyhow::Error> {

    ctx.say(&msg).await?;
    ctx.data().tx.send(CMessage { channel: ctx.channel_id(), message: msg })?;

    Ok(())
}

struct Handler {
    options: poise::FrameworkOptions<Data, anyhow::Error>,
    data: Data,
    shard_manager: std::sync::Mutex<Option<std::sync::Arc<tokio::sync::Mutex<serenity::ShardManager>>>>,
    bot_id: RwLock<Option<UserId>>,
}

struct Data {
    config: Config,
    tx: broadcast::Sender<CMessage>,
}

#[derive(Debug, Clone)]
struct CMessage {
    channel: serenity::ChannelId,
    message: String
}

#[serenity::async_trait]
impl serenity::EventHandler for Handler {
    async fn ready(&self, ctx: serenity::Context, ready: Ready) {
        let user_id = ctx.http.get_current_user().await.unwrap().id;
        let _ = self.bot_id.write().await.insert(user_id);
        info!("Discord connection ready");
        info!("Starting IRC connection...");
        tokio::spawn(irc(ctx.http.clone(), self.data.config.guild_id, self.data.tx.subscribe()));
        self.dispatch_poise_event(&ctx, &poise::Event::Ready { data_about_bot: ready }).await;

        poise::builtins::register_in_guild(ctx.http, &self.options.commands, GuildId(self.data.config.guild_id)).await.unwrap();
    }

    async fn interaction_create(&self, ctx: serenity::Context, interaction: Interaction) {
        self.dispatch_poise_event(&ctx, &poise::Event::InteractionCreate { interaction }).await;
    }
}
impl Handler {
    async fn dispatch_poise_event(&self, ctx: &serenity::Context, event: &poise::Event<'_>) {
        let shard_manager = (*self.shard_manager.lock().unwrap()).clone().unwrap();
        let framework_data = poise::FrameworkContext {
            bot_id: self.bot_id.read().await.unwrap_or_else(|| UserId(0)),
            options: &self.options,
            user_data: &self.data,
            shard_manager: &shard_manager,
        };
        poise::dispatch_event(framework_data, ctx, event).await;
    }
}

async fn irc(http: Arc<Http>, guild_id: u64, rx: broadcast::Receiver<CMessage>) {
    match listen_irc(http, guild_id, rx).await {
        Ok(_) => info!("listen_irc exited"),
        Err(e) => error!("listen_irc error: {}", e)
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
