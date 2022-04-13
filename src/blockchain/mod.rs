use thiserror::Error;

use crate::config::{genesis, TOTAL_SUPPLY};
use crate::core::{Account, Address, Block, Header, Transaction, TransactionData};
use crate::db::{KvStore, KvStoreError, RamMirrorKvStore, StringKey, WriteOp};
use crate::wallet::Wallet;

#[derive(Error, Debug)]
pub enum BlockchainError {
    #[error("kvstore error happened")]
    KvStoreError(#[from] KvStoreError),
    #[error("transaction signature is invalid")]
    SignatureError,
    #[error("balance insufficient")]
    BalanceInsufficient,
    #[error("inconsistency error")]
    Inconsistency,
    #[error("block not found")]
    BlockNotFound,
    #[error("cannot extend from the genesis block")]
    ExtendFromGenesis,
    #[error("cannot extend from very future blocks")]
    ExtendFromFuture,
    #[error("block number invalid")]
    InvalidBlockNumber,
    #[error("parent hash invalid")]
    InvalidParentHash,
    #[error("merkle root invalid")]
    InvalidMerkleRoot,
    #[error("transaction nonce invalid")]
    InvalidTransactionNonce,
}

pub trait Blockchain {
    fn get_account(&self, addr: Address) -> Result<Account, BlockchainError>;
    fn will_extend(&self, headers: &Vec<Header>) -> Result<bool, BlockchainError>;
    fn extend(&mut self, from: usize, blocks: &Vec<Block>) -> Result<(), BlockchainError>;
    fn draft_block(
        &self,
        mempool: &Vec<Transaction>,
        wallet: &Wallet,
    ) -> Result<Block, BlockchainError>;
    fn get_height(&self) -> Result<usize, BlockchainError>;
    fn get_headers(
        &self,
        since: usize,
        until: Option<usize>,
    ) -> Result<Vec<Header>, BlockchainError>;
    fn get_blocks(&self, since: usize, until: Option<usize>)
        -> Result<Vec<Block>, BlockchainError>;
}

pub struct KvStoreChain<K: KvStore> {
    database: K,
}

impl<K: KvStore> KvStoreChain<K> {
    pub fn new(kv_store: K) -> Result<KvStoreChain<K>, BlockchainError> {
        let mut chain = KvStoreChain::<K> { database: kv_store };
        if chain.get_height()? == 0 {
            chain.apply_block(&genesis::get_genesis_block())?;
        }
        Ok(chain)
    }

    fn fork_on_ram<'a>(&'a self) -> KvStoreChain<RamMirrorKvStore<'a, K>> {
        KvStoreChain {
            database: RamMirrorKvStore::new(&self.database),
        }
    }

    fn get_block(&self, index: usize) -> Result<Block, BlockchainError> {
        if index >= self.get_height()? {
            return Err(BlockchainError::BlockNotFound);
        }
        let block_key: StringKey = format!("block_{:010}", index).into();
        Ok(match self.database.get(block_key.clone())? {
            Some(b) => b.try_into()?,
            None => {
                return Err(BlockchainError::Inconsistency);
            }
        })
    }

    fn apply_tx(&mut self, tx: &Transaction) -> Result<(), BlockchainError> {
        let mut ops = Vec::new();
        if !tx.verify_signature() {
            return Err(BlockchainError::SignatureError);
        }
        match &tx.data {
            TransactionData::RegularSend { dst, amount } => {
                let mut acc_src = self.get_account(tx.src.clone())?;

                if tx.nonce != acc_src.nonce + 1 {
                    return Err(BlockchainError::InvalidTransactionNonce);
                }

                if acc_src.balance < amount + tx.fee {
                    return Err(BlockchainError::BalanceInsufficient);
                }

                acc_src.balance -= if *dst != tx.src { *amount } else { 0 } + tx.fee;
                acc_src.nonce += 1;

                ops.push(WriteOp::Put(
                    format!("account_{}", tx.src).into(),
                    acc_src.into(),
                ));

                if *dst != tx.src {
                    let mut acc_dst = self.get_account(dst.clone())?;
                    acc_dst.balance += amount;

                    ops.push(WriteOp::Put(
                        format!("account_{}", dst).into(),
                        acc_dst.into(),
                    ));
                }
            }
            _ => {
                unimplemented!();
            }
        }
        self.database.update(&ops)?;
        Ok(())
    }

    pub fn rollback_block(&mut self) -> Result<(), BlockchainError> {
        let height = self.get_height()?;
        let rollback_key: StringKey = format!("rollback_{:010}", height - 1).into();
        let mut rollback: Vec<WriteOp> = match self.database.get(rollback_key.clone())? {
            Some(b) => b.try_into()?,
            None => {
                return Err(BlockchainError::Inconsistency);
            }
        };
        rollback.push(WriteOp::Remove(format!("block_{:010}", height - 1).into()));
        rollback.push(WriteOp::Remove(format!("merkle_{:010}", height - 1).into()));
        rollback.push(WriteOp::Remove(
            format!("rollback_{:010}", height - 1).into(),
        ));
        self.database.update(&rollback)?;
        Ok(())
    }

    fn select_transactions(
        &self,
        txs: &Vec<Transaction>,
    ) -> Result<Vec<Transaction>, BlockchainError> {
        let mut fork = self.fork_on_ram();
        let mut result = Vec::new();
        for tx in txs.iter() {
            if fork.apply_tx(tx).is_ok() {
                result.push(tx.clone());
            }
        }
        Ok(result)
    }

    fn apply_block(&mut self, block: &Block) -> Result<(), BlockchainError> {
        let curr_height = self.get_height()?;

        if curr_height > 0 {
            let last_block = self.get_block(curr_height - 1)?;

            if block.header.number as usize != curr_height {
                return Err(BlockchainError::InvalidBlockNumber);
            }

            if block.header.parent_hash != last_block.header.hash() {
                return Err(BlockchainError::InvalidParentHash);
            }

            if block.header.block_root != block.merkle_tree().root() {
                return Err(BlockchainError::InvalidMerkleRoot);
            }
        }

        let mut fork = self.fork_on_ram();
        for tx in block.body.iter() {
            fork.apply_tx(tx)?;
        }
        let mut changes = fork.database.to_ops();

        changes.push(WriteOp::Put("height".into(), (curr_height + 1).into()));

        changes.push(WriteOp::Put(
            format!("rollback_{:010}", block.header.number).into(),
            self.database.rollback_of(&changes)?.into(),
        ));
        changes.push(WriteOp::Put(
            format!("block_{:010}", block.header.number).into(),
            block.into(),
        ));
        changes.push(WriteOp::Put(
            format!("merkle_{:010}", block.header.number).into(),
            block.merkle_tree().into(),
        ));

        self.database.update(&changes)?;
        Ok(())
    }
}

impl<K: KvStore> Blockchain for KvStoreChain<K> {
    fn get_account(&self, addr: Address) -> Result<Account, BlockchainError> {
        let k = format!("account_{}", addr).into();
        Ok(match self.database.get(k)? {
            Some(b) => b.try_into()?,
            None => Account {
                balance: if addr == Address::Treasury {
                    TOTAL_SUPPLY
                } else {
                    0
                },
                nonce: 0,
            },
        })
    }
    fn will_extend(&self, _headers: &Vec<Header>) -> Result<bool, BlockchainError> {
        Ok(false)
    }
    fn extend(&mut self, from: usize, blocks: &Vec<Block>) -> Result<(), BlockchainError> {
        let curr_height = self.get_height()?;

        if from == 0 {
            return Err(BlockchainError::ExtendFromGenesis);
        } else if from > curr_height {
            return Err(BlockchainError::ExtendFromFuture);
        }

        let mut forked = self.fork_on_ram();

        while forked.get_height()? > from {
            forked.rollback_block()?;
        }

        for block in blocks.iter() {
            forked.apply_block(block)?;
        }
        let ops = forked.database.to_ops();

        self.database.update(&ops)?;
        Ok(())
    }
    fn get_height(&self) -> Result<usize, BlockchainError> {
        Ok(match self.database.get("height".into())? {
            Some(b) => b.try_into()?,
            None => 0,
        })
    }
    fn get_headers(
        &self,
        since: usize,
        until: Option<usize>,
    ) -> Result<Vec<Header>, BlockchainError> {
        Ok(self
            .get_blocks(since, until)?
            .into_iter()
            .map(|b| b.header)
            .collect())
    }
    fn get_blocks(
        &self,
        since: usize,
        until: Option<usize>,
    ) -> Result<Vec<Block>, BlockchainError> {
        let mut blks: Vec<Block> = Vec::new();
        let height = self.get_height()?;
        for i in since..until.unwrap_or(height) {
            if i >= height {
                break;
            }
            blks.push(
                self.database
                    .get(format!("block_{:010}", i).into())?
                    .ok_or(BlockchainError::Inconsistency)?
                    .try_into()?,
            );
        }
        Ok(blks)
    }
    fn draft_block(
        &self,
        mempool: &Vec<Transaction>,
        _wallet: &Wallet,
    ) -> Result<Block, BlockchainError> {
        let height = self.get_height()?;
        let last_block = self.get_block(height - 1)?;
        let mut blk = Block {
            header: Default::default(),
            body: self.select_transactions(mempool)?,
        };
        blk.header.number = height as u64;
        blk.header.parent_hash = last_block.header.hash();
        blk.header.block_root = blk.merkle_tree().root();
        self.fork_on_ram().apply_block(&blk)?; // Check if everything is ok
        Ok(blk)
    }
}
