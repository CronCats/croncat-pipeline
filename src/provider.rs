#![allow(dead_code)]

use color_eyre::Result;
use futures::{stream::FuturesUnordered, Stream, StreamExt};
use std::{fmt::Debug, pin::Pin, sync::Arc};
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::util::LastSeen;

///
/// Status of a provider.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderStatus {
    Running,
    Stopped,
}

///
/// State of a provider stream.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderState<T>
where
    T: PartialOrd + Ord,
{
    pub name: String,
    pub status: ProviderStatus,
    pub last_seen: Option<LastSeen<T>>,
}

impl<T> ProviderState<T>
where
    T: PartialOrd + Ord,
{
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: ProviderStatus::Stopped,
            last_seen: None,
        }
    }
}

///
/// Provide a stream of values.
///
pub struct Provider<T, S>
where
    T: Ord,
    S: Stream<Item = Result<T>> + Send,
{
    pub(crate) state: Arc<Mutex<ProviderState<T>>>,
    pub(crate) stream: Pin<Box<S>>,
    pub(crate) outbound: mpsc::UnboundedSender<T>,
    pub(crate) shutdown: broadcast::Receiver<()>,
}

impl<T, S> Provider<T, S>
where
    T: Ord + Clone + Debug + Send + Sync + 'static,
    S: Stream<Item = Result<T>> + Send,
{
    pub fn new(
        name: String,
        outbound: mpsc::UnboundedSender<T>,
        shutdown: broadcast::Receiver<()>,
        stream: S,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(ProviderState::new(name))),
            outbound,
            shutdown,
            stream: Box::pin(stream),
        }
    }

    pub async fn run(
        state: Arc<Mutex<ProviderState<T>>>,
        mut stream: Pin<Box<S>>,
        outbound: mpsc::UnboundedSender<T>,
    ) -> Result<()> {
        while let Some(item) = stream.next().await {
            match item {
                Ok(item) => {
                    let mut state = state.lock().await;
                    state.status = ProviderStatus::Running;
                    outbound.send(item.clone())?;
                    state.last_seen = Some(LastSeen::new(item));
                }
                Err(err) => {
                    let mut state = state.lock().await;
                    state.status = ProviderStatus::Stopped;
                    Err(err)?
                }
            }
        }
        let mut state = state.lock().await;
        state.status = ProviderStatus::Stopped;
        Ok(())
    }
}

///
/// Run multiple providers concurrently.
///
pub struct ProviderSystem<T, S>
where
    T: Ord,
    S: Stream<Item = Result<T>> + Send,
{
    pub providers: Vec<Provider<T, S>>,
    pub(crate) outbound: mpsc::UnboundedSender<T>,
    pub(crate) shutdown: broadcast::Sender<()>,
}

impl<'a, T, S> ProviderSystem<T, S>
where
    T: Ord + Clone + Debug + Send + Sync + 'static,
    S: Stream<Item = Result<T>> + Send + 'static,
{
    pub fn new(outbound: mpsc::UnboundedSender<T>, shutdown: broadcast::Sender<()>) -> Self {
        Self {
            providers: Vec::new(),
            outbound,
            shutdown,
        }
    }

    pub fn add_provider_stream(&mut self, name: impl Into<String>, stream: S) {
        self.providers.push(Provider::new(
            name.into(),
            self.outbound.clone(),
            self.shutdown.subscribe(),
            stream,
        ));
    }

    pub fn get_provider_states(&self) -> Vec<Arc<Mutex<ProviderState<T>>>> {
        self.providers
            .iter()
            .map(|provider| provider.state.clone())
            .collect::<Vec<_>>()
    }

    pub async fn produce(self) -> Result<()> {
        let mut provider_futures = FuturesUnordered::new();
        let mut shutdown_rx = self.shutdown.subscribe();

        for provider in self.providers.into_iter() {
            provider_futures.push(Provider::run(
                provider.state.clone(),
                provider.stream,
                self.outbound.clone(),
            ));
        }

        let handle = tokio::spawn(async move {
            while let Some(provider) = provider_futures.next().await {
                provider?;
            }
            Ok(())
        });

        tokio::select! {
            _ = shutdown_rx.recv() => {
                Ok(())
            }
            result = handle => {
                result?
            }
        }
    }
}

///
/// State of the provider system monitor.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSystemMonitorState {
    AllProvidersStopped,
    ProblemProviders {
        lagging: Vec<String>,
        stopped: Vec<String>,
    },
    ProvidersOk,
}

///
/// Monitor the state of a provider system.
///
pub struct ProviderSystemMonitor<T>
where
    T: std::fmt::Debug + Ord,
{
    pub provider_states: Vec<Arc<Mutex<ProviderState<T>>>>,
    pub outbound: mpsc::Sender<ProviderSystemMonitorState>,
    pub state: Arc<Mutex<ProviderSystemMonitorState>>,
}

impl<T> ProviderSystemMonitor<T>
where
    T: std::fmt::Debug + PartialOrd + Ord,
{
    pub fn new(
        provider_states: Vec<Arc<Mutex<ProviderState<T>>>>,
        outbound: mpsc::Sender<ProviderSystemMonitorState>,
    ) -> Self {
        Self {
            provider_states,
            outbound,
            state: Arc::new(Mutex::new(ProviderSystemMonitorState::AllProvidersStopped)),
        }
    }

    #[mutants::skip]
    pub async fn monitor(self, interval_millis: u64) -> Result<()> {
        loop {
            let mut current_provider_states = vec![];
            for state in self.provider_states.iter() {
                let state = state.lock().await;
                current_provider_states.push(state);
            }

            let max_last_seen_value = current_provider_states
                .iter()
                .filter(|state| state.last_seen.is_some())
                .map(|state| &state.last_seen.as_ref().unwrap().value)
                .max();

            let lagging = current_provider_states
                .iter()
                .filter(|state| {
                    state.last_seen.is_some()
                        && &state.last_seen.as_ref().unwrap().value < max_last_seen_value.unwrap()
                })
                .map(|state| state.name.clone())
                .collect::<Vec<_>>();

            let stopped = current_provider_states
                .iter()
                .filter(|state| state.status == ProviderStatus::Stopped)
                .map(|state| state.name.clone())
                .collect::<Vec<_>>();

            let state = if stopped.len() == current_provider_states.len() {
                ProviderSystemMonitorState::AllProvidersStopped
            } else if !lagging.is_empty() || !stopped.is_empty() {
                ProviderSystemMonitorState::ProblemProviders { lagging, stopped }
            } else {
                ProviderSystemMonitorState::ProvidersOk
            };

            self.state.lock().await.clone_from(&state);
            self.outbound.send(state).await?;

            tokio::time::sleep(std::time::Duration::from_millis(interval_millis)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::stream;
    // use tokio::sync::broadcast;

    use super::*;

    #[tokio::test]
    async fn test_provider() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_shutdown_tx, shutdown_rx) = broadcast::channel(1);

        let provider = Provider::new(
            "test".into(),
            tx,
            shutdown_rx,
            stream::iter(vec![Ok(1), Ok(2), Ok(3)]),
        );

        tokio::spawn(async move {
            Provider::run(provider.state, provider.stream, provider.outbound)
                .await
                .unwrap();
        });

        for i in 1..=3 {
            assert_eq!(rx.recv().await.unwrap(), i);
        }
    }

    #[tokio::test]
    async fn test_provider_system() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (shutdown_tx, _shutdown_rx) = broadcast::channel(1);

        let mut provider_system = ProviderSystem::new(tx, shutdown_tx.clone());

        provider_system.add_provider_stream(
            "test1",
            stream::iter(vec![Ok(1), Ok(2), Ok(3), Ok(4), Ok(5)]),
        );
        provider_system.add_provider_stream(
            "test2",
            stream::iter(vec![Ok(6), Ok(7), Ok(8), Ok(9), Ok(10)]),
        );

        tokio::spawn(async move {
            provider_system.produce().await.unwrap();
        });

        for i in 1..=10 {
            assert_eq!(rx.recv().await.unwrap(), i);
        }
    }
}
