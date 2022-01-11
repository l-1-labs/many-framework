use crate::error;
use crate::module::ledger::list::{Transaction, TransactionContent, TransactionId};
use crate::utils::TokenAmount;
use omni::{Identity, OmniError};
use omni_abci::types::AbciCommitInfo;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::SystemTime;
use tracing::info;

/// Returns the key for the persistent kv-store.
pub(crate) fn key_for_account(id: &Identity, symbol: &str) -> Vec<u8> {
    format!("/balances/{}/{}", id.to_string(), symbol).into_bytes()
}

pub(crate) fn key_for_transaction(id: TransactionId) -> Vec<u8> {
    vec![b"/transactions/".to_vec(), id.0].concat()
}

pub struct LedgerStorage {
    symbols: BTreeSet<String>,
    minters: BTreeMap<String, Vec<Identity>>,

    persistent_store: fmerk::Merk,

    /// When this is true, we do not commit every transactions as they come,
    /// but wait for a `commit` call before committing the batch to the
    /// persistent store.
    blockchain: bool,

    latest_tid: u64,

    current_time: Option<SystemTime>,
}

impl std::fmt::Debug for LedgerStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LedgerStorage")
            .field("symbols", &self.symbols)
            .finish()
    }
}

impl LedgerStorage {
    pub fn set_time(&mut self, time: SystemTime) {
        self.current_time = Some(time);
    }

    pub fn load<P: AsRef<Path>>(persistent_path: P, blockchain: bool) -> Result<Self, String> {
        let persistent_store = fmerk::Merk::open(persistent_path).map_err(|e| e.to_string())?;

        let symbols = persistent_store
            .get(b"/config/symbols")
            .map_err(|e| e.to_string())?;
        let symbols: BTreeSet<String> = symbols
            .map_or_else(|| Ok(Default::default()), |bytes| minicbor::decode(&bytes))
            .map_err(|e| e.to_string())?;

        let minters = persistent_store
            .get(b"/config/minters")
            .map_err(|e| e.to_string())?;
        let minters = minters
            .map_or_else(|| Ok(Default::default()), |bytes| minicbor::decode(&bytes))
            .map_err(|e| e.to_string())?;

        let height = persistent_store.get(b"/height").unwrap().map_or(0u64, |x| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(x.as_slice());
            u64::from_be_bytes(bytes)
        });

        let latest_tid = height << 32;

        Ok(Self {
            symbols,
            minters,
            persistent_store,
            blockchain,
            latest_tid,
            current_time: None,
        })
    }

    pub fn new<P: AsRef<Path>>(
        symbols: BTreeSet<String>,
        initial_balances: BTreeMap<Identity, BTreeMap<String, u128>>,
        minters: BTreeMap<String, Vec<Identity>>,
        persistent_path: P,
        blockchain: bool,
    ) -> Result<Self, String> {
        let mut persistent_store = fmerk::Merk::open(persistent_path).map_err(|e| e.to_string())?;

        let mut batch: Vec<fmerk::BatchEntry> = Vec::new();

        for (k, v) in initial_balances.into_iter() {
            for (symbol, tokens) in v.into_iter() {
                if !symbols.contains(&symbol) {
                    return Err(format!(r#"Unknown symbol "{}" for identity {}"#, symbol, k));
                }

                let key = key_for_account(&k, &symbol);
                batch.push((key, fmerk::Op::Put(TokenAmount::from(tokens).to_vec())));
            }
        }

        batch.push((
            b"/config/minters".to_vec(),
            fmerk::Op::Put(minicbor::to_vec(&minters).map_err(|e| e.to_string())?),
        ));
        batch.push((
            b"/config/symbols".to_vec(),
            fmerk::Op::Put(minicbor::to_vec(&symbols).map_err(|e| e.to_string())?),
        ));

        persistent_store
            .apply(batch.as_slice())
            .map_err(|e| e.to_string())?;
        persistent_store.commit(&[]).map_err(|e| e.to_string())?;

        Ok(Self {
            symbols,
            minters,
            persistent_store,
            blockchain,
            latest_tid: 0,
            current_time: None,
        })
    }

    pub fn get_symbols(&self) -> Vec<&str> {
        self.symbols.iter().map(|x| x.as_str()).collect()
    }

    pub fn can_mint(&self, id: &Identity, symbol: &str) -> bool {
        self.minters.get(symbol).map_or(false, |x| x.contains(id))
    }

    pub fn inc_height(&mut self) -> u64 {
        let current_height = self.get_height();
        self.persistent_store
            .apply(&[(
                b"/height".to_vec(),
                fmerk::Op::Put((current_height + 1).to_be_bytes().to_vec()),
            )])
            .unwrap();
        current_height
    }
    pub fn get_height(&self) -> u64 {
        self.persistent_store
            .get(b"/height")
            .unwrap()
            .map_or(0u64, |x| {
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(x.as_slice());
                u64::from_be_bytes(bytes)
            })
    }

    fn new_transaction_id(&mut self) -> TransactionId {
        self.latest_tid += 1;
        TransactionId(self.latest_tid.to_be_bytes().to_vec())
    }

    pub fn commit(&mut self) -> AbciCommitInfo {
        let retain_height = self.inc_height();
        self.persistent_store.commit(&[]).unwrap();

        self.latest_tid = retain_height << 32;

        AbciCommitInfo {
            retain_height,
            hash: self.hash(),
        }
    }

    pub fn nb_transactions(&self) -> u64 {
        self.persistent_store
            .get(b"/transactions_count")
            .unwrap()
            .map_or(0, |x| {
                let mut bytes = [0u8; 8];
                bytes.copy_from_slice(x.as_slice());
                u64::from_be_bytes(bytes)
            })
    }
    fn add_transaction(&mut self, transaction: Transaction) -> () {
        let current_nb_transactions = self.nb_transactions();

        self.persistent_store
            .apply(&[
                (
                    key_for_transaction(transaction.id.clone()),
                    fmerk::Op::Put(minicbor::to_vec(&transaction).unwrap()),
                ),
                (
                    b"/transactions_count".to_vec(),
                    fmerk::Op::Put((current_nb_transactions + 1).to_be_bytes().to_vec()),
                ),
            ])
            .unwrap();
    }

    pub fn get_balance(&self, identity: &Identity, symbol: &str) -> TokenAmount {
        if identity.is_anonymous() {
            TokenAmount::zero()
        } else {
            let key = key_for_account(identity, symbol);
            match self.persistent_store.get(&key).unwrap() {
                None => TokenAmount::zero(),
                Some(amount) => TokenAmount::from(amount),
            }
        }
    }

    fn get_all_balances(&self, identity: &Identity) -> BTreeMap<&str, TokenAmount> {
        if identity.is_anonymous() {
            // Anonymous cannot hold funds.
            BTreeMap::new()
        } else {
            let mut result = BTreeMap::new();
            for symbol in &self.symbols {
                match self
                    .persistent_store
                    .get(&key_for_account(identity, symbol))
                {
                    Ok(None) => {}
                    Ok(Some(value)) => {
                        result.insert(symbol.as_str(), TokenAmount::from(value));
                    }
                    Err(_) => {}
                }
            }

            result
        }
    }

    pub fn get_multiple_balances(
        &self,
        identity: &Identity,
        symbols: &BTreeSet<String>,
    ) -> BTreeMap<&str, TokenAmount> {
        if symbols.is_empty() {
            self.get_all_balances(identity)
        } else {
            self.get_all_balances(identity)
                .into_iter()
                .filter(|(k, _v)| symbols.contains(*k))
                .collect()
        }
    }

    pub fn generate_proof(
        &mut self,
        identity: &Identity,
        symbols: &BTreeSet<String>,
    ) -> Result<Vec<u8>, OmniError> {
        self.persistent_store
            .prove(
                symbols
                    .iter()
                    .map(|s| key_for_account(identity, s))
                    .collect::<Vec<Vec<u8>>>()
                    .as_slice(),
            )
            .map_err(|e| OmniError::unknown(e.to_string()))
    }

    pub fn mint(
        &mut self,
        to: &Identity,
        symbol: &str,
        amount: TokenAmount,
    ) -> Result<(), OmniError> {
        if amount.is_zero() {
            // NOOP.
            return Ok(());
        }
        if to.is_anonymous() {
            return Err(error::anonymous_cannot_hold_funds());
        }

        info!("mint({}, {} {})", to, &amount, symbol);

        let mut balance = self.get_balance(to, symbol);
        balance += amount.clone();

        self.persistent_store
            .apply(&[(
                key_for_account(to, symbol),
                fmerk::Op::Put(balance.to_vec()),
            )])
            .unwrap();

        let id = self.new_transaction_id();
        self.add_transaction(Transaction {
            id,
            time: self.current_time.unwrap_or_else(SystemTime::now).into(),
            content: TransactionContent::Mint {
                account: to.clone(),
                symbol: symbol.to_string(),
                amount: amount.clone(),
            },
        });

        if !self.blockchain {
            self.persistent_store.commit(&[]).unwrap();
        }
        Ok(())
    }

    pub fn burn(
        &mut self,
        to: &Identity,
        symbol: &str,
        amount: TokenAmount,
    ) -> Result<(), OmniError> {
        if amount.is_zero() {
            // NOOP.
            return Ok(());
        }
        if to.is_anonymous() {
            return Err(error::anonymous_cannot_hold_funds());
        }

        info!("burn({}, {} {})", to, &amount, symbol);

        let mut balance = self.get_balance(to, symbol);
        balance -= amount.clone();

        self.persistent_store
            .apply(&[(
                key_for_account(to, symbol),
                fmerk::Op::Put(balance.to_vec()),
            )])
            .unwrap();

        let id = self.new_transaction_id();
        self.add_transaction(Transaction {
            id,
            time: self.current_time.unwrap_or_else(SystemTime::now).into(),
            content: TransactionContent::Burn {
                account: to.clone(),
                symbol: symbol.to_string(),
                amount: amount.clone(),
            },
        });

        if !self.blockchain {
            self.persistent_store.commit(&[]).unwrap();
        }

        Ok(())
    }

    pub fn send(
        &mut self,
        from: &Identity,
        to: &Identity,
        symbol: &str,
        amount: TokenAmount,
    ) -> Result<(), OmniError> {
        if amount.is_zero() || from == to {
            // NOOP.
            return Ok(());
        }
        if to.is_anonymous() || from.is_anonymous() {
            return Err(error::anonymous_cannot_hold_funds());
        }

        let mut amount_from = self.get_balance(from, symbol);
        if amount > amount_from {
            return Err(error::insufficient_funds());
        }

        info!("send({} => {}, {} {})", from, to, &amount, symbol);

        let mut amount_to = self.get_balance(to, symbol);
        amount_to += amount.clone();
        amount_from -= amount.clone();

        // Keys in batch must be sorted.
        let key_from = key_for_account(from, symbol);
        let key_to = key_for_account(to, symbol);

        let batch: Vec<fmerk::BatchEntry> = match key_from.cmp(&key_to) {
            Ordering::Less | Ordering::Equal => vec![
                (key_from, fmerk::Op::Put(amount_from.to_vec())),
                (key_to, fmerk::Op::Put(amount_to.to_vec())),
            ],
            _ => vec![
                (key_to, fmerk::Op::Put(amount_to.to_vec())),
                (key_from, fmerk::Op::Put(amount_from.to_vec())),
            ],
        };

        self.persistent_store.apply(&batch).unwrap();

        let id = self.new_transaction_id();
        self.add_transaction(Transaction {
            id,
            time: self.current_time.unwrap_or_else(SystemTime::now).into(),
            content: TransactionContent::Send {
                from: from.clone(),
                to: to.clone(),
                symbol: symbol.to_string(),
                amount: amount.clone(),
            },
        });

        if !self.blockchain {
            self.persistent_store.commit(&[]).unwrap();
        }

        Ok(())
    }

    pub fn hash(&self) -> Vec<u8> {
        self.persistent_store.root_hash().to_vec()
    }

    pub fn iter(&self, start: Option<TransactionId>) -> LedgerIterator {
        LedgerIterator::scoped_by_id(&self.persistent_store, start)
    }
}

pub struct LedgerIterator<'a> {
    inner: fmerk::rocksdb::DBIterator<'a>,
}

impl<'a> LedgerIterator<'a> {
    pub fn scoped_by_id(merk: &'a fmerk::Merk, start: Option<TransactionId>) -> Self {
        use fmerk::rocksdb::{Direction, IteratorMode, ReadOptions};
        let opts = ReadOptions::default();
        let start_key = start
            .map(|x| key_for_transaction(x.into()))
            .unwrap_or(b"/transactions/".to_vec());

        let mode = IteratorMode::From(&start_key, Direction::Forward);

        Self {
            inner: merk.iter_opt(mode, opts),
        }
    }
}

impl<'a> Iterator for LedgerIterator<'a> {
    type Item = (Box<[u8]>, Vec<u8>);

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|(k, v)| {
            let new_v = fmerk::tree::Tree::decode(k.to_vec(), v.as_ref());

            (k, new_v.value().to_vec())
        })
    }
}
