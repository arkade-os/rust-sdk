use crate::Error;
use std::time::Duration;

/// Current time as Unix seconds. Uses `js_sys::Date` on wasm32, `std::time` elsewhere.
pub(crate) fn unix_now() -> i64 {
    #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("valid duration")
            .as_secs() as i64
    }

    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    {
        (js_sys::Date::now() / 1000.0) as i64
    }
}

pub(crate) async fn sleep(duration: Duration) {
    #[cfg(target_arch = "wasm32")]
    {
        gloo_timers::future::sleep(duration).await
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio::time::sleep(duration).await;
    }
}

/// A utility function for running async operations with timeout and error handling
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) async fn timeout_op<F, O>(timeout: Duration, operation: F) -> Result<O, Error>
where
    F: futures_util::future::Future<Output = O>,
{
    use futures_util::future::select;
    use futures_util::future::Either;
    use gloo_timers::future::TimeoutFuture;

    let ms = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
    let timeout_future = TimeoutFuture::new(ms);

    match select(Box::pin(operation), timeout_future).await {
        Either::Left((result, _)) => Ok(result),
        Either::Right((_, _)) => Err(Error::ad_hoc(format!(
            "operation timed out after {timeout:?}"
        ))),
    }
}

#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub(crate) async fn timeout_op<F, O>(timeout: Duration, operation: F) -> Result<O, Error>
where
    F: std::future::Future<Output = O> + Send,
{
    tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| Error::ad_hoc(format!("operation timed out after {timeout:?}")))
}
