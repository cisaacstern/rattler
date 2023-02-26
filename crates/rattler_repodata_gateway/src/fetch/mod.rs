//! This module provides functionality to download and cache `repodata.json` from a remote location.

use crate::utils::{AsyncEncoding, Encoding, LockedFile};
use cache::{CacheHeaders, Expiring, RepoDataState};
use cache_control::{Cachability, CacheControl};
use futures::{future::ready, FutureExt, TryStreamExt};
use humansize::{SizeFormatter, DECIMAL};
use rattler_digest::{compute_file_digest, HashingWriter};
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client, Response, StatusCode,
};
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    time::SystemTime,
};
use tempfile::NamedTempFile;
use tokio_util::io::StreamReader;
use tracing::instrument;
use url::Url;

mod cache;

#[allow(missing_docs)]
#[derive(Debug, thiserror::Error)]
pub enum FetchRepoDataError {
    #[error("failed to acquire a lock on the repodata cache")]
    FailedToAcquireLock(#[source] anyhow::Error),

    #[error(transparent)]
    HttpError(#[from] reqwest::Error),

    #[error(transparent)]
    FailedToDownloadRepoData(std::io::Error),

    #[error("failed to create temporary file for repodata.json")]
    FailedToCreateTemporaryFile(#[source] std::io::Error),

    #[error("failed to persist temporary repodata.json file")]
    FailedToPersistTemporaryFile(#[from] tempfile::PersistError),

    #[error("failed to get metadata from repodata.json file")]
    FailedToGetMetadata(#[source] std::io::Error),

    #[error("failed to write cache state")]
    FailedToWriteCacheState(#[source] std::io::Error),

    #[error("there is no cache available")]
    NoCacheAvailable,

    #[error("the operation was cancelled")]
    Cancelled,
}

impl From<tokio::task::JoinError> for FetchRepoDataError {
    fn from(err: tokio::task::JoinError) -> Self {
        // Rethrow any panic
        if let Ok(panic) = err.try_into_panic() {
            std::panic::resume_unwind(panic);
        }

        // Otherwise it the operation has been cancelled
        FetchRepoDataError::Cancelled
    }
}

/// Defines how to use the repodata cache.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CacheAction {
    /// Use the cache if its up to date or fetch from the URL if there is no valid cached value.
    CacheOrFetch,

    /// Only use the cache, but error out if the cache is not up to date
    UseCacheOnly,

    /// Only use the cache, ignore whether or not it is up to date.
    ForceCacheOnly,

    /// Do not use the cache even if there is an up to date entry.
    NoCache,
}

impl Default for CacheAction {
    fn default() -> Self {
        CacheAction::CacheOrFetch
    }
}

/// Additional knobs that allow you to tweak the behavior of [`fetch_repo_data`].
#[derive(Default)]
pub struct FetchRepoDataOptions {
    /// How to use the cache. By default it will cache and reuse downloaded repodata.json (if the
    /// server allows it).
    pub cache_action: CacheAction,

    /// A function that is called during downloading of the repodata.json to report progress.
    pub download_progress: Option<Box<dyn FnMut(DownloadProgress)>>,
}

/// A struct that provides information about download progress.
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    /// The number of bytes already downloaded
    pub bytes: u64,

    /// The total number of bytes to download. Or `None` if this is not known. This can happen
    /// if the server does not supply a `Content-Length` header.
    pub total: Option<u64>,
}

/// The result of [`fetch_repo_data`].
#[derive(Debug)]
pub struct CachedRepoData {
    /// A lockfile that guards access to any of the repodata.json file or its cache.
    pub lock_file: LockedFile,

    /// The path to the uncompressed repodata.json file.
    pub repo_data_json_path: PathBuf,

    /// The cache data.
    pub cache_state: RepoDataState,

    /// How the cache was used for this request.
    pub cache_result: CacheResult,
}

/// Indicates whether or not the repodata.json cache was up-to-date or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheResult {
    /// The cache was hit, the data on disk was already valid.
    CacheHit,

    /// The cache was hit, we did have to check with the server, but no data was downloaded.
    CacheHitAfterFetch,

    /// The cache was present but it was outdated.
    CacheOutdated,

    /// There was no cache available
    CacheNotPresent,
}

/// Fetch the repodata.json file for the given subdirectory. The result is cached on disk using the
/// HTTP cache headers returned from the server.
///
/// The successful result of this function also returns a lockfile which ensures that both the state
/// and the repodata that is pointed to remain in sync. However, not releasing the lockfile (by
/// dropping it) could block other threads and processes, it is therefor advices to release it as
/// quickly as possible.
///
/// This method implements several different methods to download the repodata.json file from the
/// remote:
///
/// * If a `repodata.json.zst` file is available in the same directory that file is downloaded
///   and decompressed.
/// * If a `repodata.json.bz2` file is available in the same directory that file is downloaded
///   and decompressed.
/// * Otherwise the regular `repodata.json` file is downloaded.
///
/// The checks to see if a `.zst` and/or `.bz2` file exist are performed by doing a HEAD request to
/// the respective URLs. The result of these are cached.
#[instrument(err, skip_all, fields(subdir_url, cache_path = %cache_path.display()))]
pub async fn fetch_repo_data(
    subdir_url: Url,
    client: Client,
    cache_path: &Path,
    options: FetchRepoDataOptions,
) -> Result<CachedRepoData, FetchRepoDataError> {
    let subdir_url = normalize_subdir_url(subdir_url);

    // Compute the cache key from the url
    let cache_key = crate::utils::url_to_cache_filename(&subdir_url);
    let repo_data_json_path = cache_path.join(format!("{}.json", cache_key));
    let cache_state_path = cache_path.join(format!("{}.state.json", cache_key));

    // Lock all files that have to do with that cache key
    let lock_file_path = cache_path.join(format!("{}.lock", &cache_key));
    let lock_file =
        tokio::task::spawn_blocking(move || LockedFile::open_rw(lock_file_path, "repodata cache"))
            .await?
            .map_err(FetchRepoDataError::FailedToAcquireLock)?;

    // Validate the current state of the cache
    let cache_state = if options.cache_action != CacheAction::NoCache {
        let owned_subdir_url = subdir_url.clone();
        let owned_cache_path = cache_path.to_owned();
        let cache_state = tokio::task::spawn_blocking(move || {
            validate_cached_state(&owned_cache_path, &owned_subdir_url)
        })
        .await?;
        match (cache_state, options.cache_action) {
            (ValidatedCacheState::UpToDate(cache_state), _)
            | (ValidatedCacheState::OutOfDate(cache_state), CacheAction::ForceCacheOnly) => {
                // Cache is up to date or we dont care about whether or not its up to date,
                // so just immediately return what we have.
                return Ok(CachedRepoData {
                    lock_file,
                    repo_data_json_path,
                    cache_state,
                    cache_result: CacheResult::CacheHit,
                });
            }
            (ValidatedCacheState::OutOfDate(_), CacheAction::UseCacheOnly) => {
                // The cache is out of date but we also cant fetch new data
                return Err(FetchRepoDataError::NoCacheAvailable);
            }
            (ValidatedCacheState::OutOfDate(cache_state), _) => {
                // The cache is out of date but we can still refresh the data
                Some(cache_state)
            }
            (
                ValidatedCacheState::Mismatched(_),
                CacheAction::UseCacheOnly | CacheAction::ForceCacheOnly,
            ) => {
                // The cache doesnt match the repodata.json that is on disk. This means the cache is
                // not usable.
                return Err(FetchRepoDataError::NoCacheAvailable);
            }
            (ValidatedCacheState::Mismatched(cache_state), _) => {
                // The cache doesnt match the data that is on disk. but it might contain some other
                // interesting cached data as well...
                Some(cache_state)
            }
            (
                ValidatedCacheState::InvalidOrMissing,
                CacheAction::UseCacheOnly | CacheAction::ForceCacheOnly,
            ) => {
                // No cache available at all, and we cant refresh the data.
                return Err(FetchRepoDataError::NoCacheAvailable);
            }
            (ValidatedCacheState::InvalidOrMissing, _) => {
                // No cache available but we can update it!
                None
            }
        }
    } else {
        None
    };

    // Determine the availability of variants based on the cache or by querying the remote.
    let VariantAvailability {
        has_zst: cached_zst_available,
        has_bz2: cached_bz2_available,
    } = check_variant_availability(&client, &subdir_url, cache_state.as_ref()).await;

    // Now that the caches have been refreshed determine whether or not we can use one of the
    // variants. We dont check the expiration here since we just refreshed it.
    let has_zst = cached_zst_available
        .as_ref()
        .map(|state| state.value)
        .unwrap_or(false);
    let has_bz2 = cached_bz2_available
        .as_ref()
        .map(|state| state.value)
        .unwrap_or(false);

    // Determine which variant to download
    let repo_data_url = if has_zst {
        subdir_url.join("repodata.json.zst").unwrap()
    } else if has_bz2 {
        subdir_url.join("repodata.json.bz2").unwrap()
    } else {
        subdir_url.join("repodata.json").unwrap()
    };

    // Construct the HTTP request
    tracing::debug!("fetching '{}'", &repo_data_url);
    let request_builder = client.get(repo_data_url.clone());

    let mut headers = HeaderMap::default();

    // We can handle g-zip encoding which is often used. We could also set this option on the
    // client, but that will disable all download progress messages by `reqwest` because the
    // gzipped data is decoded on the fly and the size of the decompressed body is unknown.
    // However, we don't really care about the decompressed size but rather we'd like to know
    // the number of raw bytes that are actually downloaded.
    //
    // To do this we manually set the request header to accept gzip encoding and we use the
    // [`AsyncEncoding`] trait to perform the decoding on the fly.
    headers.insert(
        reqwest::header::ACCEPT_ENCODING,
        HeaderValue::from_static("gzip"),
    );

    // Add previous cache headers if we have them
    if let Some(cache_headers) = cache_state.as_ref().map(|state| &state.cache_headers) {
        cache_headers.add_to_request(&mut headers)
    }

    // Send the request and wait for a reply
    let response = request_builder
        .headers(headers)
        .send()
        .await?
        .error_for_status()?;

    // If the content didn't change, simply return whatever we have on disk.
    if response.status() == StatusCode::NOT_MODIFIED {
        tracing::debug!("repodata was unmodified");

        // Update the cache on disk with any new findings.
        let cache_state = RepoDataState {
            url: repo_data_url,
            has_zst: cached_zst_available,
            has_bz2: cached_bz2_available,
            .. cache_state.expect("we must have had a cache, otherwise we wouldn't know the previous state of the cache")
        };

        let cache_state = tokio::task::spawn_blocking(move || {
            cache_state
                .to_path(&cache_state_path)
                .map(|_| cache_state)
                .map_err(FetchRepoDataError::FailedToWriteCacheState)
        })
        .await??;

        return Ok(CachedRepoData {
            lock_file,
            repo_data_json_path,
            cache_state,
            cache_result: CacheResult::CacheHitAfterFetch,
        });
    }

    // Get cache headers from the response
    let cache_headers = CacheHeaders::from(&response);

    // Stream the content to a temporary file
    let (temp_file, blake2_hash) = stream_and_decode_to_file(
        response,
        if has_zst {
            Encoding::Zst
        } else if has_bz2 {
            Encoding::Bz2
        } else {
            Encoding::Passthrough
        },
        cache_path,
        options.download_progress,
    )
    .await?;

    // Persist the file to its final destination
    let repo_data_destination_path = repo_data_json_path.clone();
    let repo_data_json_metadata = tokio::task::spawn_blocking(move || {
        let file = temp_file
            .persist(repo_data_destination_path)
            .map_err(FetchRepoDataError::FailedToPersistTemporaryFile)?;

        // Determine the last modified date and size of the repodata.json file. We store these values in
        // the cache to link the cache to the corresponding repodata.json file.
        file.metadata()
            .map_err(FetchRepoDataError::FailedToGetMetadata)
    })
    .await??;

    // Update the cache on disk.
    let had_cache = cache_state.is_some();
    let new_cache_state = RepoDataState {
        url: repo_data_url,
        cache_headers,
        cache_last_modified: repo_data_json_metadata
            .modified()
            .map_err(FetchRepoDataError::FailedToGetMetadata)?,
        cache_size: repo_data_json_metadata.len(),
        blake2_hash: Some(blake2_hash),
        has_zst: cached_zst_available,
        has_bz2: cached_bz2_available,
        // We dont do anything with JLAP so just copy over the value.
        has_jlap: cache_state.and_then(|state| state.has_jlap),
    };

    let new_cache_state = tokio::task::spawn_blocking(move || {
        new_cache_state
            .to_path(&cache_state_path)
            .map(|_| new_cache_state)
            .map_err(FetchRepoDataError::FailedToWriteCacheState)
    })
    .await??;

    Ok(CachedRepoData {
        lock_file,
        repo_data_json_path,
        cache_state: new_cache_state,
        cache_result: if had_cache {
            CacheResult::CacheOutdated
        } else {
            CacheResult::CacheNotPresent
        },
    })
}

/// Streams and decodes the response to a new temporary file in the given directory. While writing
/// to disk it also computes the BLAKE2 hash of the file.
#[instrument(skip_all)]
async fn stream_and_decode_to_file(
    response: Response,
    content_encoding: Encoding,
    temp_dir: &Path,
    mut progress: Option<Box<dyn FnMut(DownloadProgress)>>,
) -> Result<(NamedTempFile, blake2::digest::Output<blake2::Blake2s256>), FetchRepoDataError> {
    // Determine the length of the response in bytes and notify the listener that a download is
    // starting. The response may be compressed. Decompression happens below.
    let content_size = response.content_length();
    if let Some(progress) = progress.as_mut() {
        progress(DownloadProgress {
            bytes: 0,
            total: content_size,
        })
    }

    // Determine the encoding of the response
    let transfer_encoding = Encoding::from(&response);

    // Convert the response into a byte stream
    let bytes_stream = response
        .bytes_stream()
        .map_err(|e| std::io::Error::new(ErrorKind::Other, e));

    // Listen in on the bytes as they come from the response. Progress is tracked here instead of
    // after decoding because that doesnt properly represent the number of bytes that are being
    // transferred over the network.
    let mut total_bytes = 0;
    let total_bytes_mut = &mut total_bytes;
    let bytes_stream = bytes_stream.inspect_ok(move |bytes| {
        *total_bytes_mut += bytes.len() as u64;
        if let Some(progress) = progress.as_mut() {
            progress(DownloadProgress {
                bytes: *total_bytes_mut,
                total: content_size,
            })
        }
    });

    // Create a new stream from the byte stream that decodes the bytes using the transfer encoding
    // on the fly.
    let decoded_byte_stream = StreamReader::new(bytes_stream).decode(transfer_encoding);

    // Create yet another stream that decodes the bytes yet again but this time using the content
    // encoding.
    let mut decoded_repo_data_json_bytes =
        tokio::io::BufReader::new(decoded_byte_stream).decode(content_encoding);

    tracing::trace!(
        "decoding repodata (content: {:?}, transfer: {:?})",
        content_encoding,
        transfer_encoding
    );

    // Construct a temporary file
    let temp_file =
        NamedTempFile::new_in(temp_dir).map_err(FetchRepoDataError::FailedToCreateTemporaryFile)?;

    // Clone the file handle and create a hashing writer so we can compute a hash while the content
    // is being written to disk.
    let file = tokio::fs::File::from_std(temp_file.as_file().try_clone().unwrap());
    let mut hashing_file_writer = HashingWriter::<_, blake2::Blake2s256>::new(file);

    // Decode, hash and write the data to the file.
    let bytes = tokio::io::copy(&mut decoded_repo_data_json_bytes, &mut hashing_file_writer)
        .await
        .map_err(FetchRepoDataError::FailedToDownloadRepoData)?;

    // Finalize the hash
    let (_, hash) = hashing_file_writer.finalize();

    tracing::debug!(
        "downloaded {}, decoded that into {}, BLAKE2 hash: {:x}",
        SizeFormatter::new(total_bytes, DECIMAL),
        SizeFormatter::new(bytes, DECIMAL),
        hash
    );

    Ok((temp_file, hash))
}

/// Describes the availability of certain `repodata.json`.
#[derive(Debug)]
struct VariantAvailability {
    has_zst: Option<Expiring<bool>>,
    has_bz2: Option<Expiring<bool>>,
}

/// Determine the availability of `repodata.json` variants (like a `.zst` or `.bz2`) by checking
/// a cache or the internet.
async fn check_variant_availability(
    client: &Client,
    subdir_url: &Url,
    cache_state: Option<&RepoDataState>,
) -> VariantAvailability {
    // Determine from the cache which variant are available. This is currently cached for a maximum
    // of 14 days.
    let expiration_duration = chrono::Duration::days(14);
    let has_zst = cache_state
        .and_then(|state| state.has_zst.as_ref())
        .and_then(|value| value.value(expiration_duration))
        .copied();
    let has_bz2 = cache_state
        .and_then(|state| state.has_bz2.as_ref())
        .and_then(|value| value.value(expiration_duration))
        .copied();

    // Create a future to possibly refresh the zst state.
    let zst_repodata_url = subdir_url.join("repodata.json.zst").unwrap();
    let bz2_repodata_url = subdir_url.join("repodata.json.bz2").unwrap();
    let zst_future = match has_zst {
        Some(_) => {
            // The last cached value was value so we simply copy that
            ready(cache_state.and_then(|state| state.has_zst.clone())).left_future()
        }
        None => async {
            Some(Expiring {
                value: check_valid_download_target(&zst_repodata_url, client).await,
                last_checked: chrono::Utc::now(),
            })
        }
        .right_future(),
    };

    // Create a future to determine if bz2 is available. We only check this if we dont already know that
    // zst is available because if thats available we're going to use that anyway.
    let bz2_future = if has_zst != Some(true) {
        // If the zst variant might not be available we need to check whether bz2 is available.
        async {
            match has_bz2 {
                Some(_) => {
                    // The last cached value was value so we simply copy that.
                    cache_state.and_then(|state| state.has_bz2.clone())
                }
                None => Some(Expiring {
                    value: check_valid_download_target(&bz2_repodata_url, client).await,
                    last_checked: chrono::Utc::now(),
                }),
            }
        }
        .left_future()
    } else {
        // If we already know that zst is available we simply copy the availability value from the last
        // time we checked.
        ready(cache_state.and_then(|state| state.has_zst.clone())).right_future()
    };

    // TODO: Implement JLAP

    // Await both futures so they happen concurrently. Note that a request might not actually happen if
    // the cache is still valid.
    let (has_zst, has_bz2) = futures::join!(zst_future, bz2_future);

    VariantAvailability { has_zst, has_bz2 }
}

/// Performs a HEAD request on the given URL to see if it is available.
async fn check_valid_download_target(url: &Url, client: &Client) -> bool {
    tracing::debug!("checking availability of '{url}'");

    // Otherwise, perform a HEAD request to determine whether the url seems valid.
    match client.head(url.clone()).send().await {
        Ok(response) => {
            if response.status().is_success() {
                tracing::debug!("'{url}' seems to be available");
                true
            } else {
                tracing::debug!("'{url}' seems to be unavailable");
                false
            }
        }
        Err(e) => {
            tracing::warn!(
                "failed to perform HEAD request on '{url}': {e}. Assuming its unavailable.."
            );
            false
        }
    }
}

// Ensures that the URL contains a trailing slash. This is important for the [`Url::join`] function.
fn normalize_subdir_url(url: Url) -> Url {
    let mut path = url.path();
    path = path.trim_end_matches('/');
    let mut url = url.clone();
    url.set_path(&format!("{path}/"));
    url
}

/// A value returned from [`validate_cached_state`] which indicates the state of a repodata.json cache.
enum ValidatedCacheState {
    /// There is no cache, the cache could not be parsed, or the cache does not reference the same
    /// request. We can completely ignore any cached data.
    InvalidOrMissing,

    /// The cache does not match the repodata.json file that is on disk. This usually indicates that the
    /// repodata.json was modified without updating the cache.
    Mismatched(RepoDataState),

    /// The cache could be read and corresponds to the repodata.json file that is on disk but the cached
    /// data is (partially) out of date.
    OutOfDate(RepoDataState),

    /// The cache is up to date.
    UpToDate(RepoDataState),
}

/// Tries to determine if the cache state for the repodata.json for the given `subdir_url` is
/// considered to be up-to-date.
///
/// This functions reads multiple files from the `cache_path`, it is left up to the user to ensure
/// that these files stay synchronized during the execution of this function.
fn validate_cached_state(cache_path: &Path, subdir_url: &Url) -> ValidatedCacheState {
    let cache_key = crate::utils::url_to_cache_filename(subdir_url);
    let repo_data_json_path = cache_path.join(format!("{}.json", cache_key));
    let cache_state_path = cache_path.join(format!("{}.state.json", cache_key));

    // Check if we have cached repodata.json file
    let json_metadata = match std::fs::metadata(&repo_data_json_path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return ValidatedCacheState::InvalidOrMissing,
        Err(e) => {
            tracing::warn!(
                "failed to get metadata of repodata.json file '{}': {e}. Ignoring cached files...",
                repo_data_json_path.display()
            );
            return ValidatedCacheState::InvalidOrMissing;
        }
        Ok(metadata) => metadata,
    };

    // Try to read the repodata state cache
    let cache_state = match RepoDataState::from_path(&cache_state_path) {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // Ignore, the cache just doesnt exist
            tracing::debug!("repodata cache state is missing. Ignoring cached files...");
            return ValidatedCacheState::InvalidOrMissing;
        }
        Err(e) => {
            // An error occured while reading the cached state.
            tracing::warn!(
                "invalid repodata cache state '{}': {e}. Ignoring cached files...",
                cache_state_path.display()
            );
            return ValidatedCacheState::InvalidOrMissing;
        }
        Ok(state) => state,
    };

    // Do the URLs match?
    let cached_subdir_url = if cache_state.url.path().ends_with('/') {
        cache_state.url.clone()
    } else {
        let path = cache_state.url.path();
        let (subdir_path, _) = path.rsplit_once('/').unwrap_or(("", path));
        let mut url = cache_state.url.clone();
        url.set_path(&format!("{subdir_path}/"));
        url
    };
    if &cached_subdir_url != subdir_url {
        tracing::warn!(
            "cache state refers to a different repodata.json url. Ignoring cached files..."
        );
        return ValidatedCacheState::InvalidOrMissing;
    }

    // Determine last modified date of the repodata.json file.
    let cache_last_modified = match json_metadata.modified() {
        Err(_) => {
            tracing::warn!("could not determine last modified date of repodata.json file. Ignoring cached files...");
            return ValidatedCacheState::Mismatched(cache_state);
        }
        Ok(last_modified) => last_modified,
    };

    // Make sure that the repodata state cache refers to the repodata that exists on disk.
    //
    // Check the blake hash of the repodata.json file if we have a similar hash in the state.
    if let Some(cached_hash) = cache_state.blake2_hash.as_ref() {
        match compute_file_digest::<blake2::Blake2s256>(&repo_data_json_path) {
            Err(e) => {
                tracing::warn!(
                    "could not compute BLAKE2 hash of repodata.json file: {e}. Ignoring cached files..."
                );
                return ValidatedCacheState::Mismatched(cache_state);
            }
            Ok(hash) => {
                if &hash != cached_hash {
                    tracing::warn!(
                    "BLAKE2 hash of repodata.json does not match cache state. Ignoring cached files..."
                );
                    return ValidatedCacheState::Mismatched(cache_state);
                }
            }
        }
    } else {
        // The state cache records the size and last modified date of the original file. If those do
        // not match, the repodata.json file has been modified.
        if json_metadata.len() != cache_state.cache_size
            || Some(cache_last_modified) != json_metadata.modified().ok()
        {
            tracing::warn!("repodata cache state mismatches the existing repodatajson file. Ignoring cached files...");
            return ValidatedCacheState::Mismatched(cache_state);
        }
    }

    // Determine the age of the cache
    let cache_age = match SystemTime::now().duration_since(cache_last_modified) {
        Ok(duration) => duration,
        Err(e) => {
            tracing::warn!("failed to determine cache age: {e}. Ignoring cached files...");
            return ValidatedCacheState::Mismatched(cache_state);
        }
    };

    // Parse the cache control header, and determine if the cache is out of date or not.
    match cache_state.cache_headers.cache_control.as_deref() {
        Some(cache_control) => match CacheControl::from_value(cache_control) {
            None => {
                tracing::warn!(
                "could not parse cache_control from repodata cache state. Ignoring cached files..."
            );
                return ValidatedCacheState::Mismatched(cache_state);
            }
            Some(CacheControl {
                cachability: Some(Cachability::Public),
                max_age: Some(duration),
                ..
            }) => {
                if cache_age > duration {
                    tracing::debug!(
                        "Cache is {} old but can at most be {} old. Assuming out of date...",
                        humantime::format_duration(cache_age),
                        humantime::format_duration(duration),
                    );
                    return ValidatedCacheState::OutOfDate(cache_state);
                }
            }
            Some(_) => {
                tracing::debug!(
                    "Unsupported cache-control value '{}'. Assuming out of date...",
                    cache_control
                );
                return ValidatedCacheState::OutOfDate(cache_state);
            }
        },
        None => {
            tracing::warn!("previous cache state does not contain cache_control header. Assuming out of date...");
            return ValidatedCacheState::OutOfDate(cache_state);
        }
    }

    // Well then! If we get here, it means the cache must be up to date!
    ValidatedCacheState::UpToDate(cache_state)
}

#[cfg(test)]
mod test {
    use super::{
        fetch_repo_data, CacheResult, CachedRepoData, DownloadProgress, FetchRepoDataOptions,
    };
    use crate::utils::simple_channel_server::SimpleChannelServer;
    use crate::utils::Encoding;
    use assert_matches::assert_matches;
    use hex_literal::hex;
    use reqwest::Client;
    use std::path::Path;
    use tempfile::TempDir;
    use tokio::io::AsyncWriteExt;
    use url::Url;

    async fn write_encoded(
        mut input: &[u8],
        destination: &Path,
        encoding: Encoding,
    ) -> Result<(), std::io::Error> {
        // Open the file for writing
        let mut file = tokio::fs::File::create(destination).await.unwrap();

        match encoding {
            Encoding::Passthrough => {
                tokio::io::copy(&mut input, &mut file).await?;
            }
            Encoding::GZip => {
                let mut encoder = async_compression::tokio::write::GzipEncoder::new(file);
                tokio::io::copy(&mut input, &mut encoder).await?;
                encoder.shutdown().await?;
            }
            Encoding::Bz2 => {
                let mut encoder = async_compression::tokio::write::BzEncoder::new(file);
                tokio::io::copy(&mut input, &mut encoder).await?;
                encoder.shutdown().await?;
            }
            Encoding::Zst => {
                let mut encoder = async_compression::tokio::write::ZstdEncoder::new(file);
                tokio::io::copy(&mut input, &mut encoder).await?;
                encoder.shutdown().await?;
            }
        }

        Ok(())
    }

    #[test]
    pub fn test_normalize_url() {
        assert_eq!(
            super::normalize_subdir_url(Url::parse("http://localhost/channels/empty").unwrap()),
            Url::parse("http://localhost/channels/empty/").unwrap(),
        );
        assert_eq!(
            super::normalize_subdir_url(Url::parse("http://localhost/channels/empty/").unwrap()),
            Url::parse("http://localhost/channels/empty/").unwrap(),
        );
    }

    const FAKE_REPO_DATA: &str = r#"{
        "packages.conda": {
            "asttokens-2.2.1-pyhd8ed1ab_0.conda": {
                "arch": null,
                "build": "pyhd8ed1ab_0",
                "build_number": 0,
                "build_string": "pyhd8ed1ab_0",
                "constrains": [],
                "depends": [
                    "python >=3.5",
                    "six"
                ],
                "fn": "asttokens-2.2.1-pyhd8ed1ab_0.conda",
                "license": "Apache-2.0",
                "license_family": "Apache",
                "md5": "bf7f54dd0f25c3f06ecb82a07341841a",
                "name": "asttokens",
                "noarch": "python",
                "platform": null,
                "sha256": "7ed530efddd47a96c11197906b4008405b90e3bc2f4e0df722a36e0e6103fd9c",
                "size": 27831,
                "subdir": "noarch",
                "timestamp": 1670264089059,
                "track_features": "",
                "url": "https://conda.anaconda.org/conda-forge/noarch/asttokens-2.2.1-pyhd8ed1ab_0.conda",
                "version": "2.2.1"
            }
        }
    }
    "#;

    #[tracing_test::traced_test]
    #[tokio::test]
    pub async fn test_fetch_repo_data() {
        // Create a directory with some repodata.
        let subdir_path = TempDir::new().unwrap();
        std::fs::write(subdir_path.path().join("repodata.json"), FAKE_REPO_DATA).unwrap();
        let server = SimpleChannelServer::new(subdir_path.path());

        // Download the data from the channel with an empty cache.
        let cache_dir = TempDir::new().unwrap();
        let result = fetch_repo_data(
            server.url(),
            Client::default(),
            cache_dir.path(),
            Default::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            result.cache_state.blake2_hash.unwrap()[..],
            hex!("791749939c9d6e26801bbcd525b908da15d42d3249f01efaca1ed1133f38bb87")[..]
        );
        assert_eq!(
            std::fs::read_to_string(result.repo_data_json_path).unwrap(),
            FAKE_REPO_DATA
        );
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    pub async fn test_cache_works() {
        // Create a directory with some repodata.
        let subdir_path = TempDir::new().unwrap();
        std::fs::write(subdir_path.path().join("repodata.json"), FAKE_REPO_DATA).unwrap();
        let server = SimpleChannelServer::new(subdir_path.path());

        // Download the data from the channel with an empty cache.
        let cache_dir = TempDir::new().unwrap();
        let CachedRepoData { cache_result, .. } = fetch_repo_data(
            server.url(),
            Client::default(),
            cache_dir.path(),
            Default::default(),
        )
        .await
        .unwrap();

        assert_matches!(cache_result, CacheResult::CacheNotPresent);

        // Download the data from the channel with a filled cache.
        let CachedRepoData { cache_result, .. } = fetch_repo_data(
            server.url(),
            Client::default(),
            cache_dir.path(),
            Default::default(),
        )
        .await
        .unwrap();

        assert_matches!(
            cache_result,
            CacheResult::CacheHit | CacheResult::CacheHitAfterFetch
        );

        // I know this is terrible but without the sleep rust is too blazingly fast and the server
        // doesnt think the file was actually updated.. This is because the time send by the server
        // has seconds precision.
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        // Update the original repodata.json file
        std::fs::write(subdir_path.path().join("repodata.json"), FAKE_REPO_DATA).unwrap();

        // Download the data from the channel with a filled cache.
        let CachedRepoData { cache_result, .. } = fetch_repo_data(
            server.url(),
            Client::default(),
            cache_dir.path(),
            Default::default(),
        )
        .await
        .unwrap();

        assert_matches!(cache_result, CacheResult::CacheOutdated);
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    pub async fn test_zst_works() {
        let subdir_path = TempDir::new().unwrap();
        write_encoded(
            FAKE_REPO_DATA.as_bytes(),
            &subdir_path.path().join("repodata.json.zst"),
            Encoding::Zst,
        )
        .await
        .unwrap();

        let server = SimpleChannelServer::new(subdir_path.path());

        // Download the data from the channel with an empty cache.
        let cache_dir = TempDir::new().unwrap();
        let result = fetch_repo_data(
            server.url(),
            Client::default(),
            cache_dir.path(),
            Default::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(result.repo_data_json_path).unwrap(),
            FAKE_REPO_DATA
        );
        assert_matches!(
            result.cache_state.has_zst, Some(super::Expiring {
                value, ..
            }) if value
        );
        assert_matches!(
            result.cache_state.has_bz2, Some(super::Expiring {
                value, ..
            }) if !value
        );
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    pub async fn test_bz2_works() {
        let subdir_path = TempDir::new().unwrap();
        write_encoded(
            FAKE_REPO_DATA.as_bytes(),
            &subdir_path.path().join("repodata.json.bz2"),
            Encoding::Bz2,
        )
        .await
        .unwrap();

        let server = SimpleChannelServer::new(subdir_path.path());

        // Download the data from the channel with an empty cache.
        let cache_dir = TempDir::new().unwrap();
        let result = fetch_repo_data(
            server.url(),
            Client::default(),
            cache_dir.path(),
            Default::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(result.repo_data_json_path).unwrap(),
            FAKE_REPO_DATA
        );
        assert_matches!(
            result.cache_state.has_zst, Some(super::Expiring {
                value, ..
            }) if !value
        );
        assert_matches!(
            result.cache_state.has_bz2, Some(super::Expiring {
                value, ..
            }) if value
        );
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    pub async fn test_zst_is_preferred() {
        let subdir_path = TempDir::new().unwrap();
        write_encoded(
            FAKE_REPO_DATA.as_bytes(),
            &subdir_path.path().join("repodata.json.bz2"),
            Encoding::Bz2,
        )
        .await
        .unwrap();
        write_encoded(
            FAKE_REPO_DATA.as_bytes(),
            &subdir_path.path().join("repodata.json.zst"),
            Encoding::Zst,
        )
        .await
        .unwrap();

        let server = SimpleChannelServer::new(subdir_path.path());

        // Download the data from the channel with an empty cache.
        let cache_dir = TempDir::new().unwrap();
        let result = fetch_repo_data(
            server.url(),
            Client::default(),
            cache_dir.path(),
            Default::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(result.repo_data_json_path).unwrap(),
            FAKE_REPO_DATA
        );
        assert!(result.cache_state.url.path().ends_with("repodata.json.zst"));
        assert_matches!(
            result.cache_state.has_zst, Some(super::Expiring {
                value, ..
            }) if value
        );
        assert_matches!(
            result.cache_state.has_bz2, Some(super::Expiring {
                value, ..
            }) if value
        );
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    pub async fn test_gzip_transfer_encoding() {
        // Create a directory with some repodata.
        let subdir_path = TempDir::new().unwrap();
        write_encoded(
            FAKE_REPO_DATA.as_ref(),
            &subdir_path.path().join("repodata.json.gz"),
            Encoding::GZip,
        )
        .await
        .unwrap();

        // The server is configured in such a way that if file `a` is requested but a file called
        // `a.gz` is available it will stream the `a.gz` file and report that its a `gzip` encoded
        // stream.
        let server = SimpleChannelServer::new(subdir_path.path());

        // Download the data from the channel
        let cache_dir = TempDir::new().unwrap();
        let result = fetch_repo_data(
            server.url(),
            Client::builder().no_gzip().build().unwrap(),
            cache_dir.path(),
            Default::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(result.repo_data_json_path).unwrap(),
            FAKE_REPO_DATA
        );
    }

    #[tracing_test::traced_test]
    #[tokio::test]
    pub async fn test_progress() {
        use std::cell::Cell;
        use std::sync::Arc;

        // Create a directory with some repodata.
        let subdir_path = TempDir::new().unwrap();
        std::fs::write(subdir_path.path().join("repodata.json"), FAKE_REPO_DATA).unwrap();
        let server = SimpleChannelServer::new(subdir_path.path());

        let last_download_progress = Arc::new(Cell::new(0));
        let last_download_progress_captured = last_download_progress.clone();
        let download_progress = move |progress: DownloadProgress| {
            last_download_progress_captured.set(progress.bytes);
            assert_eq!(progress.total, Some(1110));
        };

        // Download the data from the channel with an empty cache.
        let cache_dir = TempDir::new().unwrap();
        let _result = fetch_repo_data(
            server.url(),
            Client::default(),
            cache_dir.path(),
            FetchRepoDataOptions {
                download_progress: Some(Box::new(download_progress)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(last_download_progress.get(), 1110);
    }
}