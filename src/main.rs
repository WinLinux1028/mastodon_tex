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
    fs::{self, File},
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

    let _ = fs::remove_dir_all(&config.working_directory).await;
    fs::create_dir(&config.working_directory).await.unwrap();
    std::env::set_current_dir(&config.working_directory).unwrap();

    let client_ = client.clone();
    tokio::spawn(async move {
        let mut receiver = QUEUE.1.lock().await.take().unwrap();
        while let Some(msg) = receiver.recv().await {
            let config = config.clone();
            let client_ = client_.clone();
            tokio::spawn(async move {
                let _ = run(&config, &client_, msg).await;
            });
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

async fn run(
    config: &Config,
    client: &Mastodon,
    msg: Message,
) -> Result<(), Box<dyn std::error::Error>> {
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
    let base_dir = format!("request_{}", num);

    fs::create_dir(&base_dir).await?;
    scopeguard::defer! {
        let _ = std::fs::remove_dir_all(&base_dir);
    }

    let mut f = File::create(format!("{}/file.tex", &base_dir)).await?;
    f.write_all(tex.as_bytes()).await?;
    f.flush().await?;

    let mut compile = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "cd {0} && {1} file.tex && {1} file.tex && {2} file.pdf",
            &base_dir, &config.tex_compile_command, &config.pdf_png_convert_command
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    tokio::select! {
        Ok(compile) = compile.wait() => if !compile.success() {
            return Ok(())
        },
        _ = tokio::time::sleep(Duration::from_secs(config.compile_timeout_sec)) => {},
        else => return Ok(()),
    }

    let png = File::open(format!("{}/file.png", &base_dir)).await?;
    let attachment = match client.upload_media_reader(Box::new(png), None).await?.json {
        UploadMedia::Attachment(a) => a,
        UploadMedia::AsyncAttachment(a) => get_async_attachment(client, a).await,
    };

    let visibility = match &msg.visibility {
        StatusVisibility::Direct => StatusVisibility::Direct,
        _ => StatusVisibility::Unlisted,
    };

    let options = megalodon::megalodon::PostStatusInputOptions {
        media_ids: Some(vec![attachment.id.to_string()]),
        in_reply_to_id: Some(msg.id.to_string()),
        visibility: Some(visibility),
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

    let html_tag_remove = Regex::new("<.*?>").unwrap();
    let s = html_tag_remove.replace_all(&s, "");

    let s = s.replace("&lt;", "<");
    let s = s.replace("&gt;", ">");
    let s = s.replace("&amp;", "&");
    let s = s.replace("&apos;", "'");
    let s = s.replace("&quot;", "\"");

    let tex_check = Regex::new(r"(\\directlua)|(\\usepackage)|(\\end\{document\})").unwrap();
    if tex_check.find(s.as_ref()).is_some() {
        return None;
    }

    Some(s.trim().to_string())
}

#[derive(Serialize, Deserialize, Clone)]
struct Config {
    base_url: String,
    access_token: String,
    working_directory: String,
    tex_compile_command: String,
    pdf_png_convert_command: String,
    compile_timeout_sec: u64,
}
