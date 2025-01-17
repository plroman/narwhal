// Copyright(C) Facebook, Inc. and its affiliates.
use anyhow::{Context, Result};
use bytes::BufMut as _;
use bytes::BytesMut;
use clap::{crate_name, crate_version, App, AppSettings};
use env_logger::Env;
use futures::future::join_all;
use futures::sink::SinkExt as _;
use log::{info, warn};
use primary::PrimaryClientReceiverHandlerNoPrint;
use rand::Rng;
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tokio::time::{interval, sleep, Duration, Instant};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use primary::PrimaryClientReceiverHandler;
use network::Receiver;

#[tokio::main]
async fn main() -> Result<()> {
    let matches = App::new(crate_name!())
        .version(crate_version!())
        .about("Benchmark client for Narwhal and Tusk.")
        .args_from_usage("<ADDR> 'The network address of the node where to send txs'")
        .args_from_usage("--size=<INT> 'The size of each transaction in bytes'")
        .args_from_usage("--rate=<INT> 'The rate (txs/s) at which to send the transactions'")
        .args_from_usage("--nodes=[ADDR]... 'Network addresses that must be reachable before starting the benchmark.'")
        .args_from_usage("--port=<INT> 'Port to listen for batch deliveries'")
        .args_from_usage("--local 'Should run local or not'")
        .args_from_usage("--honest 'Make every sent transaction a sample transaction")
        .setting(AppSettings::ArgRequiredElseHelp)
        .get_matches();

    env_logger::Builder::from_env(Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let target = matches
        .value_of("ADDR")
        .unwrap()
        .parse::<SocketAddr>()
        .context("Invalid socket address format")?;
    let size = matches
        .value_of("size")
        .unwrap()
        .parse::<usize>()
        .context("The size of transactions must be a non-negative integer")?;
    let rate = matches
        .value_of("rate")
        .unwrap()
        .parse::<u64>()
        .context("The rate of transactions must be a non-negative integer")?;
    let nodes = matches
        .values_of("nodes")
        .unwrap_or_default()
        .into_iter()
        .map(|x| x.parse::<SocketAddr>())
        .collect::<Result<Vec<_>, _>>()
        .context("Invalid socket address format")?;
    let port = matches
        .value_of("port")
        .unwrap()
        .parse::<u16>()
        .context("The rate of transactions must be a non-negative integer")?;
    let local = matches
        .is_present("local");
    let honest = matches
        .is_present("honest");

    info!("Node address: {}", target);

    // NOTE: This log entry is used to compute performance.
    info!("Transactions size: {} B", size);

    // NOTE: This log entry is used to compute performance.
    info!("Transactions rate: {} tx/s", rate);

    info!("Local: {}", local);

    info!("Honest: {}", honest);

    let client = Client {
        target,
        size,
        rate,
        nodes,
        port,
        local,
        honest,
    };

    // Wait for all nodes to be online and synchronized.
    client.wait().await;

    // Start the benchmark.
    client.send().await.context("Failed to submit transactions")
}

struct Client {
    target: SocketAddr,
    size: usize,
    rate: u64,
    nodes: Vec<SocketAddr>,
    port: u16,
    local: bool,
    honest: bool,
}

impl Client {
    pub async fn send(&self) -> Result<()> {
        const BURST_DURATION: u64 = 1000;

        // The transaction size must be at least 16 bytes to ensure all txs are different.
        if self.size < 8 {
            return Err(anyhow::Error::msg(
                "Transaction size must be at least 8 bytes",
            ));
        }

        // Connect to the mempool.
        let stream = TcpStream::connect(self.target)
            .await
            .context(format!("failed to connect to {}", self.target))?;

        // Submit all transactions.
        let burst = self.rate;
        let mut tx = BytesMut::with_capacity(self.size);
        let mut counter = 0;
        let mut r: u32 = rand::thread_rng().gen();
        let load_client_rand: u32 = rand::thread_rng().gen();

        let mut transport = Framed::new(stream, LengthDelimitedCodec::new());
        
        let interval = interval(Duration::from_millis(BURST_DURATION));
        tokio::pin!(interval);

        let address = if self.local { 
            format!("127.0.0.1:{}", self.port)
        } else {
            format!("0.0.0.0:{}", self.port)
        }.parse().unwrap();

        if self.honest {
            Receiver::spawn(
                address,
                /* handler */
                PrimaryClientReceiverHandler {},
            );
        } else {
            Receiver::spawn(
                address,
                /* handler */
                PrimaryClientReceiverHandlerNoPrint {},
            );
        }

        // NOTE: This log entry is used to compute performance.
        info!("Start sending transactions");

        'main: loop {
            interval.as_mut().tick().await;
            let now = Instant::now();

            info!("Sending burst");

            for _ in 0..burst {
                if self.honest {
                    // NOTE: This log entry is used to compute performance.
                    info!("Sending sample transaction {}, (client {}, count {})", ((counter as u64) << 32) + load_client_rand as u64, load_client_rand, counter);

                    let mut counter = (counter as u32).to_be_bytes();
                    counter[0] = 0u8;
                    tx.put_u32(u32::from_be_bytes(counter)); // This counter identifies the tx.
                    tx.put_u32(load_client_rand) 
                } else {
                    r += 1;
                    tx.put_u32(u32::MAX);
                    tx.put_u32(r); // Ensures all clients send different txs.
                };

                tx.resize(self.size, 0u8);
                let bytes = tx.split().freeze();
                if let Err(e) = transport.send(bytes).await {
                    warn!("Failed to send transaction: {}", e);
                    break 'main;
                }
            }
            if now.elapsed().as_millis() > BURST_DURATION as u128 {
                // NOTE: This log entry is used to compute performance.
                warn!("Transaction rate too high for this client");
            }
            counter += 1;
        }
        Ok(())
    }

    pub async fn wait(&self) {
        // Wait for all nodes to be online.
        info!("Waiting for all nodes to be online...");
        join_all(self.nodes.iter().cloned().map(|address| {
            tokio::spawn(async move {
                while TcpStream::connect(address).await.is_err() {
                    sleep(Duration::from_millis(10)).await;
                }
            })
        }))
        .await;
    }
}
