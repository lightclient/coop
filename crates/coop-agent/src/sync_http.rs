use std::future::Future;
use std::time::Duration;

use reqwest::Client;

pub(crate) fn client(timeout: Duration) -> Option<Client> {
    Client::builder().timeout(timeout).build().ok()
}

pub(crate) fn run<F, T>(future: F) -> Option<T>
where
    F: Future<Output = Option<T>> + Send + 'static,
    T: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?
                .block_on(future)
        })
        .join()
        .ok()
        .flatten()
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?
            .block_on(future)
    }
}
