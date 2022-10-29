&nbsp;

<div align="center">
<img width="300" src="https://github.com/CronCats/croncat-rs/raw/main/croncat.png" />
</div>

&nbsp;

---

# croncat-pipeline

This crate exposes the block ingestion pipeline API to other crates.

## Major Parts

-   [provider.rs](./src/provider.rs) is for allowing multiple streams to run concurrently and send messages into a `tokio::sync::mpsc` unbounded channel.

-   [sequencer.rs](./src/sequencer.rs) takes in a `tokio::sync::mpsc` unbounded channel and deduplicates/ensures the order of the incoming messages.

-   [dispatcher.rs](./src/dispatcher.rs) takes in a `tokio::sync::mpsc` unbounded channel and fans the messages out to a all who subscribe to the `tokio::sync::broadcast` channel.
