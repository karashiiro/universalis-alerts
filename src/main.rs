use std::env;
use std::io::Cursor;

use crate::discord::*;
use crate::errors::*;
use crate::trigger::*;
use crate::universalis::*;
use crate::xivapi::*;
use bson::Document;
use dotenv::dotenv;
use futures_util::{pin_mut, SinkExt, StreamExt};
use mysql_async::{params, prelude::*, Pool};
use reqwest::Client;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

mod discord;
mod errors;
mod trigger;
mod universalis;
mod xivapi;

const MIN_TRIGGER_VERSION: i32 = 0;
const MAX_TRIGGER_VERSION: i32 = 0;

#[derive(Debug)]
struct UserAlert {
    name: String,
    discord_webhook: Option<String>,
    trigger: String,
}

async fn get_alerts_for_world_item(
    world_id: i32,
    item_id: i32,
    pool: &Pool,
) -> Result<Vec<(UserAlert, AlertTrigger)>> {
    // TODO: Add caching for this?
    let mut conn = pool.get_conn().await?;
    let alerts = r"SELECT `name`, `discord_webhook`, `trigger` FROM `users_alerts_next` WHERE `world_id` = :world_id AND (`item_id` = :item_id OR `item_id` = -1) AND `trigger_version` >= :min_trigger_version AND `trigger_version` <= :max_trigger_version".with(params! {
        "world_id" => world_id,
        "item_id" => item_id,
        "min_trigger_version" => MIN_TRIGGER_VERSION,
        "max_trigger_version" => MAX_TRIGGER_VERSION,
    })
        .map(&mut conn, |(name, discord_webhook, trigger)| {
            let alert = UserAlert {
                name,
                discord_webhook,
                trigger,
            };
            // TODO: Don't unwrap this
            let alert_trigger: AlertTrigger = serde_json::from_str(&alert.trigger).unwrap();
            (alert, alert_trigger)
        })
        .await?;
    Ok(alerts)
}

fn get_universalis_url(item_id: i32, world_name: &str) -> String {
    format!(
        "https://universalis.app/market/{}?server={}",
        item_id, world_name
    )
}

async fn send_discord_message(
    item_id: i32,
    world_id: i32,
    alert: &UserAlert,
    trigger: &AlertTrigger,
    trigger_result: f32,
    client: &Client,
) -> Result<()> {
    let discord_webhook = alert.discord_webhook.as_ref();
    if discord_webhook.is_none() {
        return Ok(());
    }
    let discord_webhook = discord_webhook.unwrap();

    let item = get_item(item_id, &client).await?;
    let world = get_world(world_id, &client).await?;
    let market_url = get_universalis_url(item_id, &world.name);
    let embed_title = format!("Alert triggered for {} on {}", item.name, world.name);
    let embed_footer_text = format!("universalis.app | {} | All prices include GST", alert.name);
    let embed_description = format!("One of your alerts has been triggered for the following reason(s):\n```c\n{}\n\nValue: {}```\nYou can view the item page on Universalis by clicking [this link]({}).", trigger, trigger_result, market_url);
    let payload = DiscordWebhookPayload {
        embeds: [DiscordEmbed {
            url: &market_url,
            title: &embed_title,
            description: &embed_description,
            color: 0xBD983A,
            footer: DiscordEmbedFooter {
                text: &embed_footer_text,
                icon_url: "https://universalis.app/favicon.png",
            },
            author: DiscordEmbedAuthor {
                name: "Universalis Alert!",
                icon_url: "https://cdn.discordapp.com/emojis/474543539771015168.png",
            },
        }]
        .to_vec(),
    };
    let serialized = serde_json::to_string(&payload)?;

    client
        .post(discord_webhook)
        .header("Content-Type", "application/json")
        .body(serialized)
        .send()
        .await?;

    Ok(())
}

fn parse_event_from_message(data: &[u8]) -> Result<ListingsAddEvent> {
    let mut reader = Cursor::new(data.clone());
    let document = Document::from_reader(&mut reader)?;
    let ev: ListingsAddEvent = bson::from_bson(document.into())?;
    Ok(ev)
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    // TODO: Enable tokio tracing
    // TODO: Add metrics
    // TODO: Add logging
    // TODO: Log failures instead of just yeeting errors

    let database_url = env::var("UNIVERSALIS_ALERTS_DB")?;
    let pool = Pool::new(database_url.as_str());

    let connect_addr = env::var("UNIVERSALIS_ALERTS_WS")?;
    let url = url::Url::parse(&connect_addr)?;

    // TODO: Attempt to reconnect when the connection drops?
    let (ws_stream, _) = connect_async(url).await?;
    println!("WebSocket handshake has been successfully completed");

    let (mut write, read) = ws_stream.split();

    let event = SubscribeEvent {
        event: "subscribe",
        channel: &env::var("UNIVERSALIS_ALERTS_CHANNEL")?,
    };
    let serialized = bson::to_bson(&event)?;
    let mut v: Vec<u8> = Vec::new();
    // TODO: Don't unwrap this
    serialized.as_document().unwrap().to_writer(&mut v)?;

    // TODO: Ping the connection so it doesn't die
    write.send(Message::Binary(v)).await?;

    let client = reqwest::Client::new();
    let on_message = {
        read.for_each_concurrent(None, |message| async {
            // TODO: Don't unwrap these
            let ev = message
                .chain_err(|| "failed to receive websocket message")
                .map(|m| m.into_data())
                .and_then(|data| parse_event_from_message(&data));
            if let Err(err) = ev {
                println!("{:?}", err);
                return;
            }
            let ev = ev.unwrap();

            let alerts = get_alerts_for_world_item(ev.world_id, ev.item_id, &pool)
                .await
                .unwrap();
            for (alert, trigger) in alerts {
                // Send webhook message if all trigger conditions are met
                trigger
                    .evaluate(&ev.listings)
                    .map(|tr| {
                        send_discord_message(ev.item_id, ev.world_id, &alert, &trigger, tr, &client)
                    })
                    .unwrap()
                    .await
                    .unwrap();
            }
        })
    };

    pin_mut!(on_message);
    on_message.await;

    Ok(())
}
