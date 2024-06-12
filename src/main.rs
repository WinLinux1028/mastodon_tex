use std::{process::Stdio, time::Duration};

use atomic_counter::{AtomicCounter, ConsistentCounter};
use lazy_static::lazy_static;
use megalodon::{
    entities::{
        notification::NotificationType, AsyncAttachment, Attachment, StatusVisibility, UploadMedia,
    },
    mastodon::Mastodon,
    streaming::Message,
    Megalodon,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::{
        mpsc::{self, Receiver, Sender},
        Mutex,
    },
};

const TEMPLATE: &str = include_str!("template.tex");
lazy_static! {
    static ref QUEUE: (Sender<Message>, Mutex<Option<Receiver<Message>>>) = {
        let (sender, receiver) = mpsc::channel(127);
        (sender, Mutex::new(Some(receiver)))
    };
    static ref COUNTER: ConsistentCounter = ConsistentCounter::new(0);
}

#[tokio::main]
async fn main() {
    let mut config_f = File::open("./config.json5").await.unwrap();
    let mut config = String::new();
    config_f.read_to_string(&mut config).await.unwrap();
    let config = json5::from_str::<Config>(&config).unwrap();

    let client = Mastodon::new(
        config.base_url.to_string(),
        Some(config.access_token.to_string()),
        None,
    );

    let _ = tokio::fs::remove_dir_all(&config.working_directory).await;
    tokio::fs::create_dir(&config.working_directory)
        .await
        .unwrap();
    std::env::set_current_dir(&config.working_directory).unwrap();

    let client_ = client.clone();
    tokio::spawn(async move {
        let mut receiver = QUEUE.1.lock().await.take().unwrap();
        while let Some(msg) = receiver.recv().await {
            tokio::select! {
                _ = run(&client_, msg) => {},
                _ = tokio::time::sleep(Duration::from_secs(10)) => {},
            }
        }
    });

    loop {
        client
            .user_streaming()
            .await
            .listen(Box::new(|msg| {
                let _ = QUEUE.0.try_send(msg);
            }))
            .await;

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn run(client: &Mastodon, msg: Message) -> Result<(), Box<dyn std::error::Error>> {
    let msg = match msg {
        Message::Notification(msg) => msg,
        _ => return Ok(()),
    };

    match &msg.r#type {
        NotificationType::Mention => {}
        NotificationType::Follow => {
            client
                .follow_account(msg.account.as_ref().ok_or("")?.id.clone(), None)
                .await?;
        }
        _ => return Ok(()),
    }

    let msg = msg.status.ok_or("")?;
    let content = text_clean(&msg.content).ok_or("")?;

    let command = "texgen\n";
    let texstart = content.find(command).ok_or("")? + command.len();

    let tex = format!(
        "{}\n\\begin{{document}}\n{}\n\\end{{document}}",
        TEMPLATE,
        &content[texstart..]
    );

    let num = COUNTER.inc();
    let base_name = format!("request_{}", num);
    let tex_file = format!("{}.tex", &base_name);
    scopeguard::defer! {
        let _ = Command::new("sh")
            .arg("-c")
            .arg(format!("rm -rf {}*", base_name))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    let mut f = File::create(&tex_file).await?;
    f.write_all(tex.as_bytes()).await?;
    f.flush().await?;

    let mut compile = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "lualatex --no-shell-escape --no-socket --safer --halt-on-error {} && inkscape --export-type=png --export-width=1920 --export-background=#FFFFFF --pages=1 {}.pdf",
            &tex_file, &base_name
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let compile = compile.wait().await?;
    if !compile.success() {
        return Ok(());
    }

    let png = File::open(format!("{}.png", &base_name)).await?;
    let attachment = match client.upload_media_reader(Box::new(png), None).await?.json {
        UploadMedia::Attachment(a) => a,
        UploadMedia::AsyncAttachment(a) => get_async_attachment(client, a).await,
    };

    let options = megalodon::megalodon::PostStatusInputOptions {
        media_ids: Some(vec![attachment.id.to_string()]),
        in_reply_to_id: Some(msg.id.to_string()),
        visibility: Some(StatusVisibility::Unlisted),
        ..Default::default()
    };

    client
        .post_status(format!("@{}", &msg.account.acct), Some(&options))
        .await?;

    Ok(())
}

async fn get_async_attachment(client: &Mastodon, attachment: AsyncAttachment) -> Attachment {
    loop {
        match client.get_media(attachment.id.clone()).await {
            Ok(o) => return o.json,
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        };
    }
}

fn text_clean(s: &str) -> Option<String> {
    let new_line = Regex::new("<br *?/?>").unwrap();
    let s = new_line.replace_all(s, "\n");

    let s = s.replace("&lt;", "<");
    let s = s.replace("&gt;", ">");

    let html_tag_remove = Regex::new("<.*?>").unwrap();
    let s = html_tag_remove.replace_all(&s, "");

    let tex_check = Regex::new(r"(\\directlua)|(\\end\{document\})").unwrap();
    if tex_check.find(s.as_ref()).is_some() {
        return None;
    }

    Some(s.trim().to_string())
}

#[derive(Serialize, Deserialize)]
struct Config {
    base_url: String,
    access_token: String,
    working_directory: String,
}
