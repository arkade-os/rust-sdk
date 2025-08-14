pub(crate) async fn sleep(duration: std::time::Duration) {
    #[cfg(target_arch = "wasm32")]
    {
        gloo_timers::future::sleep(duration).await
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        tokio::time::sleep(duration).await;
    }
}

/// A macro for running async operations with timeout and error handling
#[macro_export]
macro_rules! timeout_op {
    ($timeout:expr, $operation:expr) => {
        if cfg!(target_arch = "wasm32") {
            // For wasm32, we just execute without timeout
            // since browser environments typically have their own request timeouts
            $operation.await?
        } else {
            tokio::time::timeout($timeout, $operation)
                .await
                .map_err(Error::ad_hoc)??
        }
    };
}
