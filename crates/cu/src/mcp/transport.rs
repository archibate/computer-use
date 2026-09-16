use rmcp::{
    RoleServer,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};

use super::wait::AsyncWaits;
use crate::desktop::PrivateDesktop;
use tokio_util::sync::CancellationToken;

/// Close publication at transport termination, before rmcp drains handlers.
pub(super) struct WaitTransport<T> {
    inner: T,
    waits: AsyncWaits,
    desktop: PrivateDesktop,
    shutdown: CancellationToken,
}

impl<T> WaitTransport<T> {
    pub(super) fn new(
        inner: T,
        waits: AsyncWaits,
        desktop: PrivateDesktop,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            inner,
            waits,
            desktop,
            shutdown,
        }
    }

    fn stop(&self) {
        self.waits.stop();
        self.desktop.stop();
        self.shutdown.cancel();
    }
}

impl<T: Transport<RoleServer>> Transport<RoleServer> for WaitTransport<T> {
    type Error = T::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let sending = self.inner.send(item);
        let waits = self.waits.clone();
        let desktop = self.desktop.clone();
        let shutdown = self.shutdown.clone();
        async move {
            let result = sending.await;
            if result.is_err() {
                waits.stop();
                desktop.stop();
                shutdown.cancel();
            }
            result
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleServer>> {
        let message = self.inner.receive().await;
        if message.is_none() {
            self.stop();
        }
        message
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.stop();
        self.inner.close().await
    }
}

impl<T> Drop for WaitTransport<T> {
    fn drop(&mut self) {
        self.stop();
    }
}
