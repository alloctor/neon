//! AWS S3 storage wrapper around `rusoto` library.
//!
//! Respects `prefix_in_bucket` property from [`S3Config`],
//! allowing multiple api users to independently work with the same S3 bucket, if
//! their bucket prefixes are both specified and different.

use std::path::{Path, PathBuf};

use anyhow::Context;
use rusoto_core::{
    credential::{InstanceMetadataProvider, StaticProvider},
    HttpClient, Region, RusotoError,
};
use rusoto_s3::{
    DeleteObjectRequest, GetObjectError, GetObjectRequest, ListObjectsV2Request, PutObjectRequest,
    S3Client, StreamingBody, S3,
};
use tokio::{io, sync::Semaphore};
use tokio_util::io::ReaderStream;
use tracing::debug;

use crate::{
    strip_path_prefix, Download, DownloadError, RemoteObjectId, RemoteStorage, S3Config,
    REMOTE_STORAGE_PREFIX_SEPARATOR,
};

use super::StorageMetadata;

pub(super) mod metrics {
    use metrics::{register_int_counter_vec, IntCounterVec};
    use once_cell::sync::Lazy;

    static S3_REQUESTS_COUNT: Lazy<IntCounterVec> = Lazy::new(|| {
        register_int_counter_vec!(
            "remote_storage_s3_requests_count",
            "Number of s3 requests of particular type",
            &["request_type"],
        )
        .expect("failed to define a metric")
    });

    static S3_REQUESTS_FAIL_COUNT: Lazy<IntCounterVec> = Lazy::new(|| {
        register_int_counter_vec!(
            "remote_storage_s3_failures_count",
            "Number of failed s3 requests of particular type",
            &["request_type"],
        )
        .expect("failed to define a metric")
    });

    pub fn inc_get_object() {
        S3_REQUESTS_COUNT.with_label_values(&["get_object"]).inc();
    }

    pub fn inc_get_object_fail() {
        S3_REQUESTS_FAIL_COUNT
            .with_label_values(&["get_object"])
            .inc();
    }

    pub fn inc_put_object() {
        S3_REQUESTS_COUNT.with_label_values(&["put_object"]).inc();
    }

    pub fn inc_put_object_fail() {
        S3_REQUESTS_FAIL_COUNT
            .with_label_values(&["put_object"])
            .inc();
    }

    pub fn inc_delete_object() {
        S3_REQUESTS_COUNT
            .with_label_values(&["delete_object"])
            .inc();
    }

    pub fn inc_delete_object_fail() {
        S3_REQUESTS_FAIL_COUNT
            .with_label_values(&["delete_object"])
            .inc();
    }

    pub fn inc_list_objects() {
        S3_REQUESTS_COUNT.with_label_values(&["list_objects"]).inc();
    }

    pub fn inc_list_objects_fail() {
        S3_REQUESTS_FAIL_COUNT
            .with_label_values(&["list_objects"])
            .inc();
    }
}

fn download_destination(
    id: &RemoteObjectId,
    workdir: &Path,
    prefix_to_strip: Option<&str>,
) -> PathBuf {
    let path_without_prefix = match prefix_to_strip {
        Some(prefix) => id.0.strip_prefix(prefix).unwrap_or_else(|| {
            panic!(
                "Could not strip prefix '{}' from S3 object key '{}'",
                prefix, id.0
            )
        }),
        None => &id.0,
    };

    workdir.join(
        path_without_prefix
            .split(REMOTE_STORAGE_PREFIX_SEPARATOR)
            .collect::<PathBuf>(),
    )
}

/// AWS S3 storage.
pub struct S3Bucket {
    workdir: PathBuf,
    client: S3Client,
    bucket_name: String,
    prefix_in_bucket: Option<String>,
    // Every request to S3 can be throttled or cancelled, if a certain number of requests per second is exceeded.
    // Same goes to IAM, which is queried before every S3 request, if enabled. IAM has even lower RPS threshold.
    // The helps to ensure we don't exceed the thresholds.
    concurrency_limiter: Semaphore,
}

impl S3Bucket {
    /// Creates the S3 storage, errors if incorrect AWS S3 configuration provided.
    pub fn new(aws_config: &S3Config, workdir: PathBuf) -> anyhow::Result<Self> {
        debug!(
            "Creating s3 remote storage for S3 bucket {}",
            aws_config.bucket_name
        );
        let region = match aws_config.endpoint.clone() {
            Some(custom_endpoint) => Region::Custom {
                name: aws_config.bucket_region.clone(),
                endpoint: custom_endpoint,
            },
            None => aws_config
                .bucket_region
                .parse::<Region>()
                .context("Failed to parse the s3 region from config")?,
        };
        let request_dispatcher = HttpClient::new().context("Failed to create S3 http client")?;

        let access_key_id = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let secret_access_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
        // session token is used when authorizing through sso
        // which is typically the case when testing locally on developer machine
        let session_token = std::env::var("AWS_SESSION_TOKEN").ok();

        let client = if access_key_id.is_none() && secret_access_key.is_none() {
            debug!("Using IAM-based AWS access");
            S3Client::new_with(request_dispatcher, InstanceMetadataProvider::new(), region)
        } else {
            debug!(
                "Using credentials-based AWS access. Session token is set: {}",
                session_token.is_some()
            );
            S3Client::new_with(
                request_dispatcher,
                StaticProvider::new(
                    access_key_id.unwrap_or_default(),
                    secret_access_key.unwrap_or_default(),
                    session_token,
                    None,
                ),
                region,
            )
        };

        let prefix_in_bucket = aws_config.prefix_in_bucket.as_deref().map(|prefix| {
            let mut prefix = prefix;
            while prefix.starts_with(REMOTE_STORAGE_PREFIX_SEPARATOR) {
                prefix = &prefix[1..]
            }

            let mut prefix = prefix.to_string();
            while prefix.ends_with(REMOTE_STORAGE_PREFIX_SEPARATOR) {
                prefix.pop();
            }
            prefix
        });

        Ok(Self {
            client,
            workdir,
            bucket_name: aws_config.bucket_name.clone(),
            prefix_in_bucket,
            concurrency_limiter: Semaphore::new(aws_config.concurrency_limit.get()),
        })
    }

    async fn download_object(&self, request: GetObjectRequest) -> Result<Download, DownloadError> {
        let _guard = self
            .concurrency_limiter
            .acquire()
            .await
            .context("Concurrency limiter semaphore got closed during S3 download")
            .map_err(DownloadError::Other)?;

        metrics::inc_get_object();

        match self.client.get_object(request).await {
            Ok(object_output) => match object_output.body {
                None => {
                    metrics::inc_get_object_fail();
                    Err(DownloadError::Other(anyhow::anyhow!(
                        "Got no body for the S3 object given"
                    )))
                }
                Some(body) => Ok(Download {
                    metadata: object_output.metadata.map(StorageMetadata),
                    download_stream: Box::pin(io::BufReader::new(body.into_async_read())),
                }),
            },
            Err(RusotoError::Service(GetObjectError::NoSuchKey(_))) => Err(DownloadError::NotFound),
            Err(e) => {
                metrics::inc_get_object_fail();
                Err(DownloadError::Other(anyhow::anyhow!(
                    "Failed to download S3 object: {e}"
                )))
            }
        }
    }
}

#[async_trait::async_trait]
impl RemoteStorage for S3Bucket {
    fn remote_object_id(&self, local_path: &Path) -> anyhow::Result<RemoteObjectId> {
        let relative_path = strip_path_prefix(&self.workdir, local_path)?;
        let mut key = self.prefix_in_bucket.clone().unwrap_or_default();
        for segment in relative_path {
            key.push(REMOTE_STORAGE_PREFIX_SEPARATOR);
            key.push_str(&segment.to_string_lossy());
        }
        Ok(RemoteObjectId(key))
    }

    fn local_path(&self, storage_path: &RemoteObjectId) -> anyhow::Result<PathBuf> {
        Ok(download_destination(
            storage_path,
            &self.workdir,
            self.prefix_in_bucket.as_deref(),
        ))
    }

    async fn list(&self) -> anyhow::Result<Vec<RemoteObjectId>> {
        let mut document_keys = Vec::new();

        let mut continuation_token = None;
        loop {
            let _guard = self
                .concurrency_limiter
                .acquire()
                .await
                .context("Concurrency limiter semaphore got closed during S3 list")?;

            metrics::inc_list_objects();

            let fetch_response = self
                .client
                .list_objects_v2(ListObjectsV2Request {
                    bucket: self.bucket_name.clone(),
                    prefix: self.prefix_in_bucket.clone(),
                    continuation_token,
                    ..ListObjectsV2Request::default()
                })
                .await
                .map_err(|e| {
                    metrics::inc_list_objects_fail();
                    e
                })?;
            document_keys.extend(
                fetch_response
                    .contents
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|o| Some(RemoteObjectId(o.key?))),
            );

            match fetch_response.continuation_token {
                Some(new_token) => continuation_token = Some(new_token),
                None => break,
            }
        }

        Ok(document_keys)
    }

    /// See the doc for `RemoteStorage::list_prefixes`
    /// Note: it wont include empty "directories"
    async fn list_prefixes(
        &self,
        prefix: Option<&RemoteObjectId>,
    ) -> anyhow::Result<Vec<RemoteObjectId>> {
        // get the passed prefix or if it is not set use prefix_in_bucket value
        let list_prefix = prefix
            .map(|p| p.0.clone())
            .or_else(|| self.prefix_in_bucket.clone())
            .map(|mut p| {
                // required to end with a separator
                // otherwise request will return only the entry of a prefix
                if !p.ends_with(REMOTE_STORAGE_PREFIX_SEPARATOR) {
                    p.push(REMOTE_STORAGE_PREFIX_SEPARATOR);
                }
                p
            });

        let mut document_keys = Vec::new();

        let mut continuation_token = None;
        loop {
            let _guard = self
                .concurrency_limiter
                .acquire()
                .await
                .context("Concurrency limiter semaphore got closed during S3 list")?;

            metrics::inc_list_objects();

            let fetch_response = self
                .client
                .list_objects_v2(ListObjectsV2Request {
                    bucket: self.bucket_name.clone(),
                    prefix: list_prefix.clone(),
                    continuation_token,
                    delimiter: Some(REMOTE_STORAGE_PREFIX_SEPARATOR.to_string()),
                    ..ListObjectsV2Request::default()
                })
                .await
                .map_err(|e| {
                    metrics::inc_list_objects_fail();
                    e
                })?;

            document_keys.extend(
                fetch_response
                    .common_prefixes
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|o| Some(RemoteObjectId(o.prefix?))),
            );

            match fetch_response.continuation_token {
                Some(new_token) => continuation_token = Some(new_token),
                None => break,
            }
        }

        Ok(document_keys)
    }

    async fn upload(
        &self,
        from: Box<(dyn io::AsyncRead + Unpin + Send + Sync + 'static)>,
        from_size_bytes: usize,
        to: &RemoteObjectId,
        metadata: Option<StorageMetadata>,
    ) -> anyhow::Result<()> {
        let _guard = self
            .concurrency_limiter
            .acquire()
            .await
            .context("Concurrency limiter semaphore got closed during S3 upload")?;

        metrics::inc_put_object();
        self.client
            .put_object(PutObjectRequest {
                body: Some(StreamingBody::new_with_size(
                    ReaderStream::new(from),
                    from_size_bytes,
                )),
                bucket: self.bucket_name.clone(),
                key: to.0.to_owned(),
                metadata: metadata.map(|m| m.0),
                ..PutObjectRequest::default()
            })
            .await
            .map_err(|e| {
                metrics::inc_put_object_fail();
                e
            })?;
        Ok(())
    }

    async fn download(&self, from: &RemoteObjectId) -> Result<Download, DownloadError> {
        self.download_object(GetObjectRequest {
            bucket: self.bucket_name.clone(),
            key: from.0.to_owned(),
            ..GetObjectRequest::default()
        })
        .await
    }

    async fn download_byte_range(
        &self,
        from: &RemoteObjectId,
        start_inclusive: u64,
        end_exclusive: Option<u64>,
    ) -> Result<Download, DownloadError> {
        // S3 accepts ranges as https://www.w3.org/Protocols/rfc2616/rfc2616-sec14.html#sec14.35
        // and needs both ends to be exclusive
        let end_inclusive = end_exclusive.map(|end| end.saturating_sub(1));
        let range = Some(match end_inclusive {
            Some(end_inclusive) => format!("bytes={}-{}", start_inclusive, end_inclusive),
            None => format!("bytes={}-", start_inclusive),
        });

        self.download_object(GetObjectRequest {
            bucket: self.bucket_name.clone(),
            key: from.0.to_owned(),
            range,
            ..GetObjectRequest::default()
        })
        .await
    }

    async fn delete(&self, remote_object_id: &RemoteObjectId) -> anyhow::Result<()> {
        let _guard = self
            .concurrency_limiter
            .acquire()
            .await
            .context("Concurrency limiter semaphore got closed during S3 delete")?;

        metrics::inc_delete_object();

        self.client
            .delete_object(DeleteObjectRequest {
                bucket: self.bucket_name.clone(),
                key: remote_object_id.0.to_owned(),
                ..DeleteObjectRequest::default()
            })
            .await
            .map_err(|e| {
                metrics::inc_delete_object_fail();
                e
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_download_destination() -> anyhow::Result<()> {
        let workdir = tempdir()?.path().to_owned();
        let local_path = workdir.join("one").join("two").join("test_name");
        let relative_path = local_path.strip_prefix(&workdir)?;

        let key = RemoteObjectId(format!(
            "{}{}",
            REMOTE_STORAGE_PREFIX_SEPARATOR,
            relative_path
                .iter()
                .map(|segment| segment.to_str().unwrap())
                .collect::<Vec<_>>()
                .join(&REMOTE_STORAGE_PREFIX_SEPARATOR.to_string()),
        ));

        assert_eq!(
            local_path,
            download_destination(&key, &workdir, None),
            "Download destination should consist of s3 path joined with the workdir prefix"
        );

        Ok(())
    }

    #[test]
    fn storage_path_positive() -> anyhow::Result<()> {
        let workdir = tempdir()?.path().to_owned();

        let segment_1 = "matching";
        let segment_2 = "file";
        let local_path = &workdir.join(segment_1).join(segment_2);

        let storage = dummy_storage(workdir);

        let expected_key = RemoteObjectId(format!(
            "{}{REMOTE_STORAGE_PREFIX_SEPARATOR}{segment_1}{REMOTE_STORAGE_PREFIX_SEPARATOR}{segment_2}",
            storage.prefix_in_bucket.as_deref().unwrap_or_default(),
        ));

        let actual_key = storage
            .remote_object_id(local_path)
            .expect("Matching path should map to S3 path normally");
        assert_eq!(
            expected_key,
            actual_key,
            "S3 key from the matching path should contain all segments after the workspace prefix, separated with S3 separator"
        );

        Ok(())
    }

    #[test]
    fn storage_path_negatives() -> anyhow::Result<()> {
        #[track_caller]
        fn storage_path_error(storage: &S3Bucket, mismatching_path: &Path) -> String {
            match storage.remote_object_id(mismatching_path) {
                Ok(wrong_key) => panic!(
                    "Expected path '{}' to error, but got S3 key: {:?}",
                    mismatching_path.display(),
                    wrong_key,
                ),
                Err(e) => e.to_string(),
            }
        }

        let workdir = tempdir()?.path().to_owned();
        let storage = dummy_storage(workdir.clone());

        let error_message = storage_path_error(&storage, &workdir);
        assert!(
            error_message.contains("Prefix and the path are equal"),
            "Message '{}' does not contain the required string",
            error_message
        );

        let mismatching_path = PathBuf::from("somewhere").join("else");
        let error_message = storage_path_error(&storage, &mismatching_path);
        assert!(
            error_message.contains(mismatching_path.to_str().unwrap()),
            "Error should mention wrong path"
        );
        assert!(
            error_message.contains(workdir.to_str().unwrap()),
            "Error should mention server workdir"
        );
        assert!(
            error_message.contains("is not prefixed with"),
            "Message '{}' does not contain a required string",
            error_message
        );

        Ok(())
    }

    #[test]
    fn local_path_positive() -> anyhow::Result<()> {
        let workdir = tempdir()?.path().to_owned();
        let storage = dummy_storage(workdir.clone());
        let timeline_dir = workdir.join("timelines").join("test_timeline");
        let relative_timeline_path = timeline_dir.strip_prefix(&workdir)?;

        let s3_key = create_s3_key(
            &relative_timeline_path.join("not a metadata"),
            storage.prefix_in_bucket.as_deref(),
        );
        assert_eq!(
            download_destination(&s3_key, &workdir, storage.prefix_in_bucket.as_deref()),
            storage
                .local_path(&s3_key)
                .expect("For a valid input, valid S3 info should be parsed"),
            "Should be able to parse metadata out of the correctly named remote delta file"
        );

        let s3_key = create_s3_key(
            &relative_timeline_path.join("metadata"),
            storage.prefix_in_bucket.as_deref(),
        );
        assert_eq!(
            download_destination(&s3_key, &workdir, storage.prefix_in_bucket.as_deref()),
            storage
                .local_path(&s3_key)
                .expect("For a valid input, valid S3 info should be parsed"),
            "Should be able to parse metadata out of the correctly named remote metadata file"
        );

        Ok(())
    }

    #[test]
    fn download_destination_matches_original_path() -> anyhow::Result<()> {
        let workdir = tempdir()?.path().to_owned();
        let original_path = workdir
            .join("timelines")
            .join("some_timeline")
            .join("some name");

        let dummy_storage = dummy_storage(workdir);

        let key = dummy_storage.remote_object_id(&original_path)?;
        let download_destination = dummy_storage.local_path(&key)?;

        assert_eq!(
            original_path, download_destination,
            "'original path -> storage key -> matching fs path' transformation should produce the same path as the input one for the correct path"
        );

        Ok(())
    }

    fn dummy_storage(workdir: PathBuf) -> S3Bucket {
        S3Bucket {
            workdir,
            client: S3Client::new("us-east-1".parse().unwrap()),
            bucket_name: "dummy-bucket".to_string(),
            prefix_in_bucket: Some("dummy_prefix/".to_string()),
            concurrency_limiter: Semaphore::new(1),
        }
    }

    fn create_s3_key(relative_file_path: &Path, prefix: Option<&str>) -> RemoteObjectId {
        RemoteObjectId(relative_file_path.iter().fold(
            prefix.unwrap_or_default().to_string(),
            |mut path_string, segment| {
                path_string.push(REMOTE_STORAGE_PREFIX_SEPARATOR);
                path_string.push_str(segment.to_str().unwrap());
                path_string
            },
        ))
    }
}
