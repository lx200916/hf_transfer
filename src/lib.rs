use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use rand::{thread_rng, Rng};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_RANGE, RANGE};
use std::collections::HashMap;
use std::io::SeekFrom;
use std::sync::Arc;

use std::fs::remove_file;
use std::path::Path;
use tokio::io::AsyncSeekExt;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tokio::time::sleep;

/// parallel_failures:  Number of maximum failures of different chunks in parallel (cannot exceed max_files)
/// max_retries: Number of maximum attempts per chunk. (Retries are exponentially backed off + jitter)
/// number of threads can be tuned by environment variable `TOKIO_WORKER_THREADS` as documented in https://docs.rs/tokio/latest/tokio/runtime/struct.Builder.html#method.worker_threads
#[pyfunction]
#[pyo3(signature = (url, filename, max_files, chunk_size, parallel_failures=0, max_retries=0, headers=None))]
fn download(
    url: String,
    filename: String,
    max_files: usize,
    chunk_size: usize,
    parallel_failures: usize,
    max_retries: usize,
    headers: Option<HashMap<String, String>>,
) -> PyResult<()> {
    if parallel_failures > max_files {
        return Err(PyException::new_err(
            "Error parallel_failures cannot be > max_files".to_string(),
        ));
    }
    if (parallel_failures == 0) != (max_retries == 0) {
        return Err(PyException::new_err(
            "For retry mechanism you need to set both `parallel_failures` and `max_retries`"
                .to_string(),
        ));
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            download_async(
                url,
                filename.clone(),
                max_files,
                chunk_size,
                parallel_failures,
                max_retries,
                headers,
            )
            .await
        })
        .map_err(|err| {
            let path = Path::new(&filename);
            if path.exists() {
                match remove_file(filename) {
                    Ok(_) => err,
                    Err(err) => {
                        return PyException::new_err(format!(
                            "Error while removing corrupted file: {err:?}"
                        ));
                    }
                }
            } else {
                err
            }
        })
}

fn jitter() -> usize {
    thread_rng().gen_range(0..=500)
}

pub fn exponential_backoff(base_wait_time: usize, n: usize, max: usize) -> usize {
    (base_wait_time + n.pow(2) + jitter()).min(max)
}

async fn download_async(
    url: String,
    filename: String,
    max_files: usize,
    chunk_size: usize,
    parallel_failures: usize,
    max_retries: usize,
    input_headers: Option<HashMap<String, String>>,
) -> PyResult<()> {
    let client = reqwest::Client::new();

    let mut headers = HeaderMap::new();
    if let Some(input_headers) = input_headers {
        for (k, v) in input_headers {
            let k: HeaderName = k
                .try_into()
                .map_err(|err| PyException::new_err(format!("Invalid header: {err:?}")))?;
            let v: HeaderValue = v
                .try_into()
                .map_err(|err| PyException::new_err(format!("Invalid header value: {err:?}")))?;
            headers.insert(k, v);
        }
    };

    let response = client
        .get(&url)
        .headers(headers.clone())
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|err| PyException::new_err(format!("Error while downloading: {err:?}")))?;

    let content_range = response
        .headers()
        .get(CONTENT_RANGE)
        .ok_or(PyException::new_err("No content length"))?
        .to_str()
        .map_err(|err| PyException::new_err(format!("Error while downloading: {err:?}")))?;

    let size: Vec<&str> = content_range.split('/').collect();
    // Content-Range: bytes 0-0/702517648
    // https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Content-Range
    let length: usize = size
        .last()
        .ok_or(PyException::new_err(
            "Error while downloading: No size was detected",
        ))?
        .parse()
        .map_err(|err| PyException::new_err(format!("Error while downloading: {err:?}")))?;

    let mut handles = vec![];
    let semaphore = Arc::new(Semaphore::new(max_files));
    let parallel_failures_semaphore = Arc::new(Semaphore::new(parallel_failures));

    let chunk_size = chunk_size;
    for start in (0..length).step_by(chunk_size) {
        let url = url.clone();
        let filename = filename.clone();
        let client = client.clone();
        let headers = headers.clone();

        let stop = std::cmp::min(start + chunk_size - 1, length);
        let permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|err| PyException::new_err(format!("Error while downloading: {err:?}")))?;
        let parallel_failures_semaphore = parallel_failures_semaphore.clone();
        handles.push(tokio::spawn(async move {
            let mut chunk = download_chunk(&client, &url, &filename, start, stop, headers.clone()).await;
            let mut i = 0;
            if parallel_failures > 0{
                while let Err(dlerr) = chunk {
                    let parallel_failure_permit = parallel_failures_semaphore.clone().try_acquire_owned().map_err(|err| {
                        PyException::new_err(format!(
                            "Failed too many failures in parallel ({parallel_failures:?}): {dlerr:?} ({err:?})"
                        ))
                    })?;

                    let wait_time = exponential_backoff(300, i, 10_000);
                    sleep(tokio::time::Duration::from_millis(wait_time as u64)).await;

                    chunk = download_chunk(&client, &url, &filename, start, stop, headers.clone()).await;
                    i += 1;
                    if i > max_retries{
                        return Err(PyException::new_err(format!(
                            "Failed after too many retries ({max_retries:?}): {dlerr:?}"
                        )));
                    }
                    drop(parallel_failure_permit);
                }
            }
            drop(permit);
            chunk
        }));
    }

    // Output the chained result
    let results: Vec<Result<PyResult<()>, tokio::task::JoinError>> =
        futures::future::join_all(handles).await;
    let results: PyResult<()> = results.into_iter().flatten().collect();
    results?;
    Ok(())
}

async fn download_chunk(
    client: &reqwest::Client,
    url: &str,
    filename: &str,
    start: usize,
    stop: usize,
    headers: HeaderMap,
) -> PyResult<()> {
    // Process each socket concurrently.
    let range = format!("bytes={start}-{stop}");
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .open(filename)
        .await
        .map_err(|err| PyException::new_err(format!("Error while downloading: {err:?}")))?;
    file.seek(SeekFrom::Start(start as u64))
        .await
        .map_err(|err| PyException::new_err(format!("Error while downloading: {err:?}")))?;
    let response = client
        .get(url)
        .headers(headers)
        .header(RANGE, range)
        .send()
        .await
        .map_err(|err| PyException::new_err(format!("Error while downloading: {err:?}")))?
        .error_for_status()
        .map_err(|err| PyException::new_err(format!("Error while downloading: {err:?}")))?;
    let content = response
        .bytes()
        .await
        .map_err(|err| PyException::new_err(format!("Error while downloading: {err:?}")))?;
    file.write_all(&content)
        .await
        .map_err(|err| PyException::new_err(format!("Error while downloading: {err:?}")))?;
    Ok(())
}

/// A Python module implemented in Rust.
#[pymodule]
fn hf_transfer(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(download, m)?)?;
    Ok(())
}
