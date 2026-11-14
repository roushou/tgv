//! Downloading a release artifact: the `Fetch` seam, and its Reqwest
//! implementation, which follows only https redirects and stops at a byte
//! limit.

use ren_depot::{job::HostFailure, source::ArtifactUrl};
use std::{future::Future, path::Path, time::Duration};
use tokio::io::AsyncWriteExt as _;

/// The most bytes a download may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteLimit(u64);

impl ByteLimit {
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    pub fn bytes(self) -> u64 {
        self.0
    }
}

/// Downloads an artifact to a file.
pub trait Fetch: Send + Sync {
    /// Write what `url` serves to `into`, failing once it passes `limit`.
    fn fetch(
        &self,
        url: &ArtifactUrl,
        into: &Path,
        limit: ByteLimit,
    ) -> impl Future<Output = Result<(), HostFailure>> + Send;
}

/// Downloads over https with Reqwest.
#[derive(Debug, Clone)]
pub struct ReqwestFetch {
    client: reqwest::Client,
}

impl ReqwestFetch {
    const REDIRECTS: usize = 10;

    pub fn new() -> Result<Self, reqwest::Error> {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            if attempt.url().scheme() != "https" {
                attempt.error("a release redirected away from https")
            } else if attempt.previous().len() >= Self::REDIRECTS {
                attempt.stop()
            } else {
                attempt.follow()
            }
        });
        let client = reqwest::Client::builder()
            .user_agent(concat!("ren/", env!("CARGO_PKG_VERSION")))
            .redirect(policy)
            .https_only(true)
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self { client })
    }
}

impl Fetch for ReqwestFetch {
    async fn fetch(
        &self,
        url: &ArtifactUrl,
        into: &Path,
        limit: ByteLimit,
    ) -> Result<(), HostFailure> {
        let mut response = self
            .client
            .get(url.as_str())
            .send()
            .await
            .map_err(|_| HostFailure::Network)?;
        if !response.status().is_success() {
            return Err(HostFailure::Refused);
        }
        if response
            .content_length()
            .is_some_and(|length| length > limit.bytes())
        {
            return Err(HostFailure::TooLarge);
        }
        let mut file = tokio::fs::File::create(into)
            .await
            .map_err(|_| HostFailure::Filesystem)?;
        let mut written: u64 = 0;
        while let Some(chunk) = response.chunk().await.map_err(|_| HostFailure::Network)? {
            written = written.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
            if written > limit.bytes() {
                return Err(HostFailure::TooLarge);
            }
            file.write_all(&chunk)
                .await
                .map_err(|_| HostFailure::Filesystem)?;
        }
        file.flush().await.map_err(|_| HostFailure::Filesystem)?;
        file.sync_all().await.map_err(|_| HostFailure::Filesystem)
    }
}
