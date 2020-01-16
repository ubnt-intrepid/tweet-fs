#![allow(clippy::unnecessary_mut_passed)]
#![deny(clippy::unimplemented)]

use polyfuse::{
    io::{Reader, Writer},
    reply::{ReplyAttr, ReplyOpen, ReplyWrite},
    Context, FileAttr, Filesystem, Operation,
};
use slab::Slab;
use std::{env, io, path::PathBuf, time::Duration};
use tokio::sync::Mutex;

const TTL: Duration = Duration::from_secs(60 * 60 * 24 * 365);
const ROOT_INO: u64 = 1;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    dotenv::dotenv()?;
    tokio_compat::run_std(async {
        if let Err(err) = run().await {
            tracing::error!("failed: {}", err);
        }
    });
    Ok(())
}

async fn run() -> anyhow::Result<()> {
    let mountpoint = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("missing mountpoint"))?;
    anyhow::ensure!(
        mountpoint.is_file(),
        "the mountpoint must be a regular file"
    );

    polyfuse_tokio::mount(TweetFS::new()?, mountpoint, &[]).await?;

    Ok(())
}

struct TweetFS {
    files: Mutex<Slab<Vec<u8>>>,
    consumer_secret: String,
    consumer_key: String,
    access_token: String,
    access_token_secret: String,
}

impl TweetFS {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            files: Mutex::default(),
            consumer_key: env::var("CONSUMER_KEY")?,
            consumer_secret: env::var("CONSUMER_SECRET")?,
            access_token: env::var("ACCESS_TOKEN")?,
            access_token_secret: env::var("ACCESS_TOKEN_SECRET")?,
        })
    }
}

#[polyfuse::async_trait]
impl Filesystem for TweetFS {
    async fn call<'a, 'cx, T: ?Sized>(
        &'a self,
        cx: &'a mut Context<'cx, T>,
        op: Operation<'cx>,
    ) -> io::Result<()>
    where
        T: Reader + Writer + Unpin + Send,
    {
        tracing::debug!("op={:?}", op);
        match op {
            Operation::Getattr(..) => {
                let mut attr = FileAttr::default();
                attr.set_mode(libc::S_IFREG as u32 | 0o200);
                attr.set_ino(ROOT_INO);
                attr.set_nlink(1);
                attr.set_uid(unsafe { libc::getuid() });
                attr.set_gid(unsafe { libc::getgid() });

                cx.reply(
                    ReplyAttr::new(attr) //
                        .ttl_attr(TTL),
                )
                .await?;

                Ok(())
            }
            Operation::Open(op) => {
                match op.flags() as libc::c_int & libc::O_ACCMODE {
                    libc::O_WRONLY => (),
                    _ => return cx.reply_err(libc::EPERM).await,
                }

                let mut files = self.files.lock().await;
                let key = files.insert(vec![]);

                cx.reply(
                    ReplyOpen::new(key as u64) //
                        .direct_io(true)
                        .keep_cache(false),
                )
                .await?;

                Ok(())
            }
            Operation::Write(op) => {
                let mut files = self.files.lock().await;
                let content = match files.get_mut(op.fh() as usize) {
                    Some(file) => file,
                    None => return cx.reply_err(libc::EIO).await,
                };

                let offset = op.offset() as usize;
                let size = op.size() as usize;
                content.resize(offset + size, 0);
                {
                    use futures::io::AsyncReadExt;
                    let mut reader = cx.reader();
                    reader
                        .read_exact(&mut content[offset..offset + size])
                        .await
                        .unwrap();
                }

                cx.reply(ReplyWrite::new(op.size())).await?;
                Ok(())
            }
            Operation::Release(op) => {
                let mut files = self.files.lock().await;

                let status = files.remove(op.fh() as usize);
                let status = String::from_utf8_lossy(&status).into_owned();

                tracing::debug!("tweet: status={:?}", status);

                use futures::compat::Future01CompatExt;
                let res = egg_mode::tweet::DraftTweet::new(&status)
                    .send(&egg_mode::Token::Access {
                        consumer: egg_mode::KeyPair {
                            key: self.consumer_key.clone().into(),
                            secret: self.consumer_secret.clone().into(),
                        },
                        access: egg_mode::KeyPair {
                            key: self.access_token.clone().into(),
                            secret: self.access_token_secret.clone().into(),
                        },
                    })
                    .compat()
                    .await;
                tracing::debug!("tweet result: {:?}", res);

                cx.reply(()).await?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
