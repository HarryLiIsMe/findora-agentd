use crate::db::{Db, Proto};
use chrono::NaiveDateTime;
use clap::{Parser, Subcommand};
use feth::{error::Result, BLOCK_TIME};
use serde::{Deserialize, Serialize};
use std::{
    fmt::{Display, Formatter},
    io::BufRead,
    path::{Path, PathBuf},
};
use web3::types::{Address, H256};

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about=None)]
pub(crate) struct Cli {
    /// The maximum parallelism
    #[clap(long, default_value_t = 200)]
    pub(crate) max_parallelism: u64,

    /// The count of transactions sent by a routine
    #[clap(long, default_value_t = 0)]
    pub(crate) count: u64,

    /// the source account file
    #[clap(long, parse(from_os_str), value_name = "FILE", default_value = "source_keys.001")]
    pub(crate) source: PathBuf,

    /// block time of the network
    #[clap(long, default_value_t = BLOCK_TIME)]
    pub(crate) block_time: u64,

    /// findora network full-node urls: http://node0:8545,http://node1:8545
    #[clap(long)]
    pub(crate) network: Option<String>,

    /// http request timeout, seconds
    #[clap(long)]
    pub(crate) timeout: Option<u64>,

    /// save metric file or not
    #[clap(long)]
    pub(crate) keep_metric: bool,

    #[clap(subcommand)]
    pub(crate) command: Option<Commands>,
}

#[allow(dead_code)]
#[derive(Debug, Default, Serialize, Deserialize)]
struct BlockInfo {
    height: u64,
    timestamp: i64,
    txs: u64,
    valid_txs: u64,
    block_time: Option<u64>,
    begin: u64,
    snapshot: u64,
    end: u64,
    commit: u64,
    commit_evm: u64,
}

impl Display for BlockInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let block_time = self.block_time.unwrap_or(0);
        write!(
            f,
            "{},{},{},{},{},{},{},{}",
            self.height, block_time, self.txs, self.begin, self.snapshot, self.end, self.commit, self.commit_evm
        )
    }
}

impl Cli {
    pub(crate) fn parse_args() -> Self {
        Cli::parse()
    }

    pub(crate) fn etl_cmd<P>(abcid: P, tendermint: P, redis: &str, load: bool) -> Result<()>
    where
        P: AsRef<Path> + std::fmt::Debug,
    {
        println!("{:?} {:?} {} {}", abcid, tendermint, redis, load);

        let proto = if &redis[..4] == "unix" { Proto::Unix } else { Proto::Url };
        let db = Db::new(Some(proto), None, redis, Some(6379), Some(0))?;
        let mut min_height = u64::MAX;
        let mut max_height = u64::MIN;

        let tm_log = std::fs::File::open(tendermint)?;
        for line in std::io::BufReader::new(tm_log).lines() {
            match line {
                Ok(l) if l.contains("Executed block") => {
                    let mut blk = (None, None, None, None);
                    // I[2022-04-07|02:17:07.759] Executed block module=state height=191 validTxs=3368 invalidTxs=666
                    // parse timestamp
                    // %Y-%m-%d|%H:%M:%S.%.3f
                    let time_str = &l[2..25];
                    blk.0 = NaiveDateTime::parse_from_str(time_str, "%Y-%m-%d|%H:%M:%S%.3f")
                        .map(|dt| dt.timestamp())
                        .ok();
                    for word in l.split_whitespace() {
                        let kv = word.split('=').collect::<Vec<_>>();
                        if kv.len() != 2 {
                            continue;
                        } else {
                            match kv[0] {
                                "height" => blk.1 = kv[1].parse::<u64>().ok(),
                                "validTxs" => blk.2 = kv[1].parse::<u64>().ok(),
                                "invalidTxs" => blk.3 = kv[1].parse::<u64>().ok(),
                                _ => {}
                            }
                        }
                    }
                    let bi = BlockInfo {
                        height: blk.1.unwrap(),
                        timestamp: blk.0.unwrap(),
                        txs: blk.2.unwrap() + blk.3.unwrap(),
                        valid_txs: blk.2.unwrap(),
                        ..Default::default()
                    };
                    if min_height > bi.height {
                        min_height = bi.height;
                    }
                    if max_height < bi.height {
                        max_height = bi.height
                    }
                    let raw_data = serde_json::to_string(&bi).unwrap();
                    db.insert(bi.height, raw_data.as_bytes())
                        .expect("failed to insert a block info");
                    //blocks.insert(bi.height, std::cell::RefCell::new(bi));
                }
                _ => {}
            }
        }

        let abci_log = std::fs::File::open(abcid)?;
        std::io::BufReader::new(abci_log)
            .lines()
            .filter_map(|line| line.map_or(None, |l| if l.contains("tps,") { Some(l) } else { None }))
            .for_each(|line| {
                let words = line[52..].split(',').collect::<Vec<_>>();
                match words.last().map(|w| w.trim()) {
                    Some("end of begin_block") => {
                        // tps,begin_block,31,31,td_height 781,end of begin_block
                        let height = words[words.len() - 2].split_whitespace().collect::<Vec<_>>()[1]
                            .parse::<u64>()
                            .unwrap();
                        if let Ok(raw_bi) = db.get(height) {
                            let mut bi: BlockInfo = serde_json::from_str(raw_bi.as_str()).unwrap();
                            bi.snapshot = words[2].parse::<u64>().unwrap();
                            bi.begin = words[3].parse::<u64>().unwrap();
                            let new_raw = serde_json::to_string(&bi).unwrap();
                            db.insert(bi.height, new_raw.as_bytes())
                                .expect("failed to update a block info");
                        }
                    }
                    Some("end of end_block") => {
                        // tps,end_block,6,td_height 781,end of end_block
                        let height = words[words.len() - 2].split_whitespace().collect::<Vec<_>>()[1]
                            .parse::<u64>()
                            .unwrap();
                        if let Ok(raw_bi) = db.get(height) {
                            let mut bi: BlockInfo = serde_json::from_str(raw_bi.as_str()).unwrap();
                            bi.end = words[2].parse::<u64>().unwrap();
                            let new_raw = serde_json::to_string(&bi).unwrap();
                            db.insert(bi.height, new_raw.as_bytes())
                                .expect("failed to update a block info");
                        }
                    }
                    Some("end of commit") => {
                        // tps,commit,2,60,62,td_height 781,end of commit
                        let height = words[words.len() - 2].split_whitespace().collect::<Vec<_>>()[1]
                            .parse::<u64>()
                            .unwrap();
                        if let Ok(raw_bi) = db.get(height) {
                            let mut bi: BlockInfo = serde_json::from_str(raw_bi.as_str()).unwrap();
                            bi.commit_evm = words[3].parse::<u64>().unwrap();
                            bi.commit = words[4].parse::<u64>().unwrap();
                            let new_raw = serde_json::to_string(&bi).unwrap();
                            db.insert(bi.height, new_raw.as_bytes())
                                .expect("failed to update a block info");
                        }
                    }
                    _ => {}
                }
            });

        for h in min_height..=max_height {
            if let Ok(bi) = db.get(h) {
                println!("{}", serde_json::from_str::<BlockInfo>(bi.as_str()).unwrap());
            }
        }
        Ok(())
    }
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Fund Ethereum accounts
    Fund {
        /// ethereum-compatible network
        #[clap(long)]
        network: String,

        /// http request timeout, seconds
        #[clap(long)]
        timeout: Option<u64>,

        /// block time of the network
        #[clap(long, default_value_t = BLOCK_TIME)]
        block_time: u64,

        /// the number of Eth Account to be fund
        #[clap(long, default_value_t = 0)]
        count: u64,

        /// how much 0.1-eth to fund
        #[clap(long, default_value_t = 1)]
        amount: u64,

        /// load keys from file
        #[clap(long)]
        load: bool,

        /// re-deposit account with insufficient balance
        #[clap(long)]
        redeposit: bool,
    },
    /// check ethereum account information
    Info {
        /// ethereum-compatible network
        #[clap(long)]
        network: String,

        /// http request timeout, seconds
        #[clap(long)]
        timeout: Option<u64>,

        /// ethereum address
        #[clap(long)]
        account: Address,
    },

    /// Transaction Operations
    Transaction {
        /// ethereum-compatible network
        #[clap(long)]
        network: String,

        /// http request timeout, seconds
        #[clap(long)]
        timeout: Option<u64>,

        /// transaction hash
        #[clap(long)]
        hash: H256,
    },

    /// Block Operations
    Block {
        /// ethereum-compatible network
        #[clap(long)]
        network: String,

        /// http request timeout, seconds
        #[clap(long)]
        timeout: Option<u64>,

        /// start block height
        #[clap(long)]
        start: Option<u64>,

        /// block count, could be less than zero
        #[clap(long)]
        count: Option<i64>,
    },

    /// ETL procession
    Etl {
        /// abcid log file
        #[clap(long)]
        abcid: String,

        /// tendermint log file
        #[clap(long)]
        tendermint: String,

        /// redis db address
        #[clap(long, default_value = "127.0.0.1")]
        redis: String,

        /// load data
        #[clap(long)]
        load: bool,
    },
}