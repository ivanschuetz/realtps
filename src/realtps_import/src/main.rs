#![allow(unused)]

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use ethers::prelude::*;
use ethers::utils::hex::ToHex;
use futures::stream::{FuturesUnordered, StreamExt};
use log::{debug, error, info, warn};
use realtps_common::{all_chains, Block, Chain, Db, JsonDb};
use serde_derive::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use structopt::StructOpt;
use tokio::runtime::Builder;
use tokio::task;
use tokio::task::JoinHandle;

mod delay;

#[derive(StructOpt, Debug)]
struct Opts {
    #[structopt(subcommand)]
    cmd: Option<Command>,
}

#[derive(StructOpt, Debug)]
enum Command {
    Run,
    Import,
    Calculate,
}

enum Job {
    Import(Chain),
    Calculate,
}

static RPC_CONFIG_PATH: &str = "rpc_config.toml";

#[derive(Deserialize, Serialize)]
struct RpcConfig {
    chains: HashMap<Chain, String>,
}

fn main() -> Result<()> {
    env_logger::init();

    let opts = Opts::from_args();
    let cmd = opts.cmd.unwrap_or(Command::Run);

    let rpc_config = load_rpc_config(RPC_CONFIG_PATH)?;

    let runtime = Builder::new_multi_thread()
        .enable_all()
        .worker_threads(4)
        .max_blocking_threads(128)
        .build()?;

    runtime.block_on(run(cmd, rpc_config))?;

    Ok(())
}

async fn run(cmd: Command, rpc_config: RpcConfig) -> Result<()> {
    let importer = make_importer(&rpc_config).await?;

    let mut jobs = FuturesUnordered::new();

    for job in init_jobs(cmd).into_iter() {
        jobs.push(importer.do_job(job));
    }

    loop {
        let job_result = jobs.next().await;
        if let Some(new_jobs) = job_result {
            for new_job in new_jobs {
                jobs.push(importer.do_job(new_job));
            }
        } else {
            error!("no more jobs?!");
            break;
        }
    }

    Ok(())
}

fn load_rpc_config<P: AsRef<Path>>(path: P) -> Result<RpcConfig> {
    let rpc_config_file = fs::read_to_string(path).context("unable to load RPC configuration")?;
    let rpc_config = toml::from_str::<RpcConfig>(&rpc_config_file)
        .context("unable to parse RPC configuration")?;

    Ok(rpc_config)
}

fn print_error(e: &anyhow::Error) {
    error!("error: {}", e);
    let mut source = e.source();
    while let Some(source_) = source {
        error!("source: {}", source_);
        source = source_.source();
    }
}

fn init_jobs(cmd: Command) -> Vec<Job> {
    match cmd {
        Command::Run => {
            let import_jobs = init_jobs(Command::Import);
            let calculate_jobs = init_jobs(Command::Calculate);
            import_jobs
                .into_iter()
                .chain(calculate_jobs.into_iter())
                .collect()
        }
        Command::Import => all_chains().into_iter().map(Job::Import).collect(),
        Command::Calculate => {
            vec![Job::Calculate]
        }
    }
}

#[async_trait]
trait Client: Send + Sync + 'static {
    async fn client_version(&self) -> Result<String>;
    async fn get_block_number(&self) -> Result<u64>;
    async fn get_block(&self, block_number: u64) -> Result<Option<Block>>;
}

struct EthersClient {
    chain: Chain,
    provider: Provider<Http>,
}

#[async_trait]
impl Client for EthersClient {
    async fn client_version(&self) -> Result<String> {
        Ok(self.provider.client_version().await?)
    }

    async fn get_block_number(&self) -> Result<u64> {
        Ok(self.provider.get_block_number().await?.as_u64())
    }

    async fn get_block(&self, block_number: u64) -> Result<Option<Block>> {
        if let Some(block) = self.provider.get_block(block_number).await? {
            // I like this `map` <3
            ethers_block_to_block(self.chain, block).map(Some)
        } else {
            Ok(None)
        }
    }
}

type SolanaClient = RpcClient;

#[async_trait]
impl Client for SolanaClient {
    async fn client_version(&self) -> Result<String> {
        Ok(self.get_version()?.solana_core)
    }

    async fn get_block_number(&self) -> Result<u64> {
        self.get_block_height().map_err(|e| anyhow!("{}", e))
    }

    async fn get_block(&self, block_number: u64) -> Result<Option<Block>> {
        // todo: error handling with return missing block
        // `ClientResult<EncodedConfirmedBlock>`

        let block = self.get_block(block_number)?;
        solana_block_to_block(block).map(Some)
    }
}

async fn make_importer(rpc_config: &RpcConfig) -> Result<Importer> {
    let clients = make_all_clients(rpc_config).await?;

    Ok(Importer {
        db: Arc::new(Box::new(JsonDb)),
        clients,
    })
}

async fn make_all_clients(rpc_config: &RpcConfig) -> Result<HashMap<Chain, Box<dyn Client>>> {
    let mut client_futures = vec![];
    for chain in all_chains() {
        let rpc_url = get_rpc_url(&chain, rpc_config).to_string();
        let client_future = task::spawn(make_client(chain, rpc_url));
        client_futures.push((chain, client_future));
    }

    let mut clients = HashMap::new();

    for (chain, client_future) in client_futures {
        let client = client_future.await??;
        clients.insert(chain, client);
    }

    Ok(clients)
}

async fn make_client(chain: Chain, rpc_url: String) -> Result<Box<dyn Client>> {
    info!("creating client for {} at {}", chain, rpc_url);

    match chain {
        Chain::Arbitrum
        | Chain::Avalanche
        | Chain::Binance
        | Chain::Celo
        | Chain::Cronos
        | Chain::Ethereum
        | Chain::Fuse
        | Chain::Fantom
        | Chain::Harmony
        | Chain::Heco
        | Chain::KuCoin
        | Chain::Moonriver
        | Chain::OKEx
        | Chain::Polygon
        | Chain::Rootstock
        | Chain::Telos
        | Chain::XDai => {
            let provider = Provider::<Http>::try_from(rpc_url)?;
            let client = EthersClient { chain, provider };

            let version = client.client_version().await?;
            info!("node version for {}: {}", chain, version);

            Ok(Box::new(client))
        }
        Chain::Solana => {
            let client = Box::new(SolanaClient::new(rpc_url));

            let version = client.client_version().await?;
            info!("node version for Solana: {}", version);

            Ok(client)
        }
        _ => unreachable!(),
    }
}

fn get_rpc_url<'a>(chain: &Chain, rpc_config: &'a RpcConfig) -> &'a str {
    if let Some(url) = rpc_config.chains.get(chain) {
        return url;
    } else {
        todo!()
    }
}

struct Importer {
    db: Arc<Box<dyn Db>>,
    clients: HashMap<Chain, Box<dyn Client>>,
}

impl Importer {
    async fn do_job(&self, job: Job) -> Vec<Job> {
        let r = match job {
            Job::Import(chain) => self.import(chain).await,
            Job::Calculate => self.calculate().await,
        };

        match r {
            Ok(new_jobs) => new_jobs,
            Err(e) => {
                print_error(&e);
                error!("error running job. repeating");
                delay::job_error_delay().await;
                vec![job]
            }
        }
    }

    async fn import(&self, chain: Chain) -> Result<Vec<Job>> {
        info!("beginning import for {}", chain);

        let client = self.clients.get(&chain).expect("client");

        let head_block_number = client.get_block_number().await?;
        let head_block_number = head_block_number;
        debug!("head block number for {}: {}", chain, head_block_number);

        let highest_block_number = self.db.load_highest_block_number(chain)?;

        if let Some(highest_block_number) = highest_block_number {
            debug!(
                "highest block number for {}: {}",
                chain, highest_block_number
            );
            if head_block_number < highest_block_number {
                warn!(
                    "head_block_number < highest_block_number for chain {}. head: {}; highest: {}",
                    chain, head_block_number, highest_block_number
                )
            } else {
                let needed_blocks = head_block_number - highest_block_number;
                info!("importing {} blocks for {}", needed_blocks, chain);
            }
        } else {
            info!("no highest block number for {}", chain);
        }

        if Some(head_block_number) != highest_block_number {
            let initial_sync = highest_block_number.is_none();
            const INITIAL_SYNC_BLOCKS: u64 = 100;
            let mut synced = 0;

            let mut block_number = head_block_number;

            loop {
                debug!("fetching block {} for {}", block_number, chain);

                let block = loop {
                    let block = client.get_block(block_number).await?;

                    if let Some(block) = block {
                        break block;
                    } else {
                        debug!(
                            "received no block for number {} on chain {}",
                            block_number, chain
                        );
                        delay::retry_delay().await;
                    }
                };

                let parent_hash = block.parent_hash.clone();

                let db = self.db.clone();
                task::spawn_blocking(move || db.store_block(block)).await??;

                synced += 1;

                if initial_sync && synced == INITIAL_SYNC_BLOCKS {
                    info!("finished initial sync for {}", chain);
                    break;
                }

                if let Some(prev_block_number) = block_number.checked_sub(1) {
                    let db = self.db.clone();
                    let prev_block =
                        task::spawn_blocking(move || db.load_block(chain, prev_block_number))
                            .await??;

                    if let Some(prev_block) = prev_block {
                        if prev_block.hash != parent_hash {
                            warn!(
                                "reorg of chain {} at block {}; old hash: {}; new hash: {}",
                                chain, prev_block_number, prev_block.hash, parent_hash
                            );
                            // continue - have wrong version of prev block
                        } else {
                            if let Some(highest_block_number) = highest_block_number {
                                if prev_block_number <= highest_block_number {
                                    info!(
                                        "completed import of chain {} to block {} / {}",
                                        chain, prev_block_number, parent_hash
                                    );
                                    break;
                                } else {
                                    warn!(
                                        "found incomplete previous import for {} at block {}",
                                        chain, prev_block_number
                                    );
                                    // Found a run of blocks from a previous incomplete import.
                                    // Keep going and overwrite them.
                                    // continue
                                }
                            } else {
                                warn!(
                                    "found incomplete previous import for {} at block {}",
                                    chain, prev_block_number
                                );
                                // Found a run of blocks from a previous incomplete import.
                                // Keep going and overwrite them.
                                // continue
                            }
                        }
                    } else {
                        // continue - don't have the prev block
                    }

                    debug!("still need block {} for {}", prev_block_number, chain);
                    block_number = prev_block_number;

                    delay::courtesy_delay().await;

                    continue;
                } else {
                    info!("completed import of chain {} to genesis", chain);
                    break;
                }
            }

            let db = self.db.clone();
            task::spawn_blocking(move || db.store_highest_block_number(chain, head_block_number))
                .await??;
        } else {
            info!("no new blocks for {}", chain);
        }

        delay::rescan_delay().await;

        Ok(vec![Job::Import(chain)])
    }

    async fn calculate(&self) -> Result<Vec<Job>> {
        info!("beginning tps calculation");
        let tasks: Vec<(Chain, JoinHandle<Result<ChainCalcs>>)> = all_chains()
            .into_iter()
            .map(|chain| {
                let calc_future = calculate_for_chain(self.db.clone(), chain);
                (chain, task::spawn(calc_future))
            })
            .collect();

        for (chain, task) in tasks {
            let res = task.await?;
            match res {
                Ok(calcs) => {
                    info!("calculated {} tps for chain {}", calcs.tps, calcs.chain);
                    let db = self.db.clone();
                    task::spawn_blocking(move || db.store_tps(calcs.chain, calcs.tps)).await??;
                }
                Err(e) => {
                    print_error(&anyhow::Error::from(e));
                    error!("error calculating for {}", chain);
                }
            }
        }

        delay::recalculate_delay().await;

        Ok(vec![Job::Calculate])
    }
}

struct ChainCalcs {
    chain: Chain,
    tps: f64,
}

async fn calculate_for_chain(db: Arc<Box<dyn Db>>, chain: Chain) -> Result<ChainCalcs> {
    let highest_block_number = {
        let db = db.clone();
        task::spawn_blocking(move || db.load_highest_block_number(chain)).await??
    };
    let highest_block_number =
        highest_block_number.ok_or_else(|| anyhow!("no data for chain {}", chain))?;

    async fn load_block_(
        db: &Arc<Box<dyn Db>>,
        chain: Chain,
        number: u64,
    ) -> Result<Option<Block>> {
        let db = db.clone();
        task::spawn_blocking(move || db.load_block(chain, number)).await?
    }

    let load_block = |number| load_block_(&db, chain, number);

    let latest_timestamp = load_block(highest_block_number)
        .await?
        .expect("first block")
        .timestamp;

    let seconds_per_week = 60 * 60 * 24 * 7;
    let min_timestamp = latest_timestamp
        .checked_sub(seconds_per_week)
        .expect("underflow");

    let mut current_block_number = highest_block_number;
    let mut current_block = load_block(current_block_number)
        .await?
        .expect("first_block");

    let mut num_txs: u64 = 0;

    let start = std::time::Instant::now();

    let mut blocks = 0;

    let init_timestamp = loop {
        let now = std::time::Instant::now();
        let duration = now - start;
        let secs = duration.as_secs();
        if secs > 0 {
            debug!("bps for {}: {:.2}", chain, blocks as f64 / secs as f64)
        }
        blocks += 1;

        assert!(current_block_number != 0);

        let prev_block_number = current_block_number - 1;
        let prev_block = load_block(prev_block_number).await?;

        if let Some(prev_block) = prev_block {
            num_txs = num_txs
                .checked_add(current_block.num_txs)
                .expect("overflow");

            if prev_block.timestamp > current_block.timestamp {
                warn!(
                    "non-monotonic timestamp in block {} for chain {}. prev: {}; current: {}",
                    current_block_number, chain, prev_block.timestamp, current_block.timestamp
                );
            }

            if prev_block.timestamp <= min_timestamp {
                break prev_block.timestamp;
            }
            if prev_block.block_number == 0 {
                break prev_block.timestamp;
            }

            current_block_number = prev_block_number;
            current_block = prev_block;
        } else {
            break current_block.timestamp;
        }
    };

    assert!(init_timestamp <= latest_timestamp);
    let total_seconds = latest_timestamp - init_timestamp;
    let total_seconds_u32 =
        u32::try_from(total_seconds).map_err(|_| anyhow!("seconds overflows u32"))?;
    let num_txs_u32 = u32::try_from(num_txs).map_err(|_| anyhow!("num txs overflows u32"))?;
    let total_seconds_f64 = f64::from(total_seconds_u32);
    let num_txs_f64 = f64::from(num_txs_u32);
    let tps = num_txs_f64 / total_seconds_f64;

    Ok(ChainCalcs { chain, tps })
}

fn ethers_block_to_block(chain: Chain, block: ethers::prelude::Block<H256>) -> Result<Block> {
    Ok(Block {
        chain,
        block_number: block.number.expect("block number").as_u64(),
        timestamp: u64::try_from(block.timestamp).map_err(|e| anyhow!("{}", e))?,
        num_txs: u64::try_from(block.transactions.len())?,
        hash: block.hash.expect("hash").encode_hex(),
        parent_hash: block.parent_hash.encode_hex(),
    })
}

fn solana_block_to_block(block: solana_transaction_status::EncodedConfirmedBlock) -> Result<Block> {
    Ok(Block {
        chain: Chain::Solana,
        block_number: block.block_height.expect("block_number"),
        timestamp: u64::try_from(block.block_time.expect("timestamp"))
            .map_err(|e| anyhow!("{}", e))?,
        num_txs: u64::try_from(block.transactions.len()).map_err(|e| anyhow!("{}", e))?,
        hash: block.blockhash,
        parent_hash: block.previous_blockhash,
    })
}
