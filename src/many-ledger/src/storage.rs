use crate::error;
use many::message::ResponseMessage;
use many::server::module::abci_backend::AbciCommitInfo;
use many::server::module::account;
use many::types::ledger::{Symbol, TokenAmount, Transaction, TransactionId, TransactionInfo};
use many::types::{CborRange, SortOrder, Timestamp};
use many::{Identity, ManyError};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, Bound};
use std::ops::RangeBounds;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tracing::info;

type TransactionStorage = account::features::multisig::InfoReturn;

const MULTISIG_DEFAULT_THRESHOLD: u64 = 1;
const MULTISIG_DEFAULT_TIMEOUT_IN_SECS: u64 = 60 * 60 * 24; // A day.
const MULTISIG_DEFAULT_EXECUTE_AUTOMATICALLY: bool = false;
const MULTISIG_MAXIMUM_TIMEOUT_IN_SECS: u64 = 185 * 60 * 60 * 24; // ~6 months.

#[derive(Clone, minicbor::Encode, minicbor::Decode)]
#[cbor(map)]
pub struct AccountStorage {
    #[n(0)]
    identity: Identity,

    #[n(1)]
    account: account::Account,

    #[n(2)]
    transactions_in_flight: Vec<account::features::multisig::InfoReturn>,
}

pub(crate) const TRANSACTIONS_ROOT: &[u8] = b"/transactions/";

// Left-shift the height by this amount of bits
const HEIGHT_TXID_SHIFT: u64 = 32;

/// Returns the key for the persistent kv-store.
pub(super) fn key_for_account_balance(id: &Identity, symbol: &Symbol) -> Vec<u8> {
    format!("/balances/{}/{}", id, symbol).into_bytes()
}

pub(crate) fn key_for_transaction(id: TransactionId) -> Vec<u8> {
    vec![TRANSACTIONS_ROOT.to_vec(), id.0.into()].concat()
}

pub(super) fn key_for_account(id: &Identity) -> Vec<u8> {
    format!("/accounts/{}", id).into_bytes()
}

pub(super) fn key_for_multisig_transaction(token: &[u8]) -> Vec<u8> {
    vec![b"/multisig/{}".as_slice(), token].concat().to_vec()
}

pub struct LedgerStorage {
    symbols: BTreeMap<Symbol, String>,
    persistent_store: fmerk::Merk,

    /// When this is true, we do not commit every transactions as they come,
    /// but wait for a `commit` call before committing the batch to the
    /// persistent store.
    blockchain: bool,

    latest_tid: TransactionId,

    current_time: Option<SystemTime>,
    current_hash: Option<Vec<u8>>,

    next_account_id: u32,
    account_identity: Identity,
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
        let symbols: BTreeMap<Symbol, String> = symbols
            .map_or_else(|| Ok(Default::default()), |bytes| minicbor::decode(&bytes))
            .map_err(|e| e.to_string())?;
        let next_account_id = persistent_store
            .get(b"/config/account_id")
            .unwrap()
            .map_or(0, |x| {
                let mut bytes = [0u8; 4];
                bytes.copy_from_slice(x.as_slice());
                u32::from_be_bytes(bytes)
            });

        let account_identity: Identity = Identity::from_bytes(
            &persistent_store
                .get(b"/config/identity")
                .expect("Could not open storage.")
                .expect("Could not find key '/config/identity' in storage."),
        )
        .map_err(|e| e.to_string())?;

        let height = persistent_store.get(b"/height").unwrap().map_or(0u64, |x| {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(x.as_slice());
            u64::from_be_bytes(bytes)
        });

        let latest_tid = TransactionId::from(height << HEIGHT_TXID_SHIFT);

        Ok(Self {
            symbols,
            persistent_store,
            blockchain,
            latest_tid,
            current_time: None,
            current_hash: None,
            next_account_id,
            account_identity,
        })
    }

    pub fn new<P: AsRef<Path>>(
        symbols: BTreeMap<Symbol, String>,
        initial_balances: BTreeMap<Identity, BTreeMap<Symbol, TokenAmount>>,
        persistent_path: P,
        identity: Identity,
        blockchain: bool,
    ) -> Result<Self, String> {
        let mut persistent_store = fmerk::Merk::open(persistent_path).map_err(|e| e.to_string())?;

        let mut batch: Vec<fmerk::BatchEntry> = Vec::new();

        for (k, v) in initial_balances.into_iter() {
            for (symbol, tokens) in v.into_iter() {
                if !symbols.contains_key(&symbol) {
                    return Err(format!(r#"Unknown symbol "{}" for identity {}"#, symbol, k));
                }

                let key = key_for_account_balance(&k, &symbol);
                batch.push((key, fmerk::Op::Put(tokens.to_vec())));
            }
        }

        batch.push((
            b"/config/identity".to_vec(),
            fmerk::Op::Put(identity.to_vec()),
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
            persistent_store,
            blockchain,
            latest_tid: TransactionId::from(vec![0]),
            current_time: None,
            current_hash: None,
            next_account_id: 0,
            account_identity: identity,
        })
    }

    pub fn get_symbols(&self) -> BTreeMap<Symbol, String> {
        self.symbols.clone()
    }

    fn inc_height(&mut self) -> u64 {
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

    fn new_account_id(&mut self) -> Identity {
        let current_id = self.next_account_id;
        self.persistent_store
            .apply(&[(
                b"/config/account_id".to_vec(),
                fmerk::Op::Put((current_id + 1).to_be_bytes().to_vec()),
            )])
            .unwrap();

        self.account_identity
            .with_subresource_id(current_id)
            .expect("Too many accounts")
    }

    fn new_transaction_id(&mut self) -> TransactionId {
        self.latest_tid += 1;
        self.latest_tid.clone()
    }

    pub fn commit(&mut self) -> AbciCommitInfo {
        let height = self.inc_height();
        let retain_height = 0;
        self.persistent_store.commit(&[]).unwrap();

        let hash = self.persistent_store.root_hash().to_vec();
        self.current_hash = Some(hash.clone());

        self.latest_tid = TransactionId::from(height << HEIGHT_TXID_SHIFT);

        AbciCommitInfo {
            retain_height,
            hash: hash.into(),
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

    fn add_transaction(&mut self, transaction: Transaction) {
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

    pub fn get_balance(&self, identity: &Identity, symbol: &Symbol) -> TokenAmount {
        if identity.is_anonymous() {
            TokenAmount::zero()
        } else {
            let key = key_for_account_balance(identity, symbol);
            match self.persistent_store.get(&key).unwrap() {
                None => TokenAmount::zero(),
                Some(amount) => TokenAmount::from(amount),
            }
        }
    }

    fn get_all_balances(&self, identity: &Identity) -> BTreeMap<&Symbol, TokenAmount> {
        if identity.is_anonymous() {
            // Anonymous cannot hold funds.
            BTreeMap::new()
        } else {
            let mut result = BTreeMap::new();
            for symbol in self.symbols.keys() {
                match self
                    .persistent_store
                    .get(&key_for_account_balance(identity, symbol))
                {
                    Ok(None) => {}
                    Ok(Some(value)) => {
                        result.insert(symbol, TokenAmount::from(value));
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
        symbols: &BTreeSet<Symbol>,
    ) -> BTreeMap<&Symbol, TokenAmount> {
        if symbols.is_empty() {
            self.get_all_balances(identity)
        } else {
            self.get_all_balances(identity)
                .into_iter()
                .filter(|(k, _v)| symbols.contains(*k))
                .collect()
        }
    }

    pub fn send(
        &mut self,
        from: &Identity,
        to: &Identity,
        symbol: &Symbol,
        amount: TokenAmount,
    ) -> Result<(), ManyError> {
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
        let key_from = key_for_account_balance(from, symbol);
        let key_to = key_for_account_balance(to, symbol);

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

        self.add_transaction(Transaction::send(
            id,
            self.current_time.unwrap_or_else(SystemTime::now),
            *from,
            *to,
            *symbol,
            amount,
        ));

        if !self.blockchain {
            self.persistent_store.commit(&[]).unwrap();
        }

        Ok(())
    }

    pub fn hash(&self) -> Vec<u8> {
        self.current_hash
            .as_ref()
            .map_or_else(|| self.persistent_store.root_hash().to_vec(), |x| x.clone())
    }

    pub fn iter(&self, range: CborRange<TransactionId>, order: SortOrder) -> LedgerIterator {
        LedgerIterator::scoped_by_id(&self.persistent_store, range, order)
    }

    pub fn add_account(&mut self, account: account::Account) -> Result<Identity, ManyError> {
        let id = self.new_account_id();
        self.commit_account(&id, account)?;
        Ok(id)
    }

    pub fn get_account(&self, id: &Identity) -> Option<account::Account> {
        self.persistent_store
            .get(&key_for_account(id))
            .unwrap_or_default()
            .as_ref()
            .and_then(|bytes| {
                minicbor::decode(bytes)
                    .map_err(|e| ManyError::deserialization_error(e.to_string()))
                    .ok()
            })
    }

    pub fn commit_account(
        &mut self,
        id: &Identity,
        account: account::Account,
    ) -> Result<(), ManyError> {
        self.persistent_store
            .apply(&[(
                key_for_account(&id),
                fmerk::Op::Put(
                    minicbor::to_vec(account)
                        .map_err(|e| ManyError::serialization_error(e.to_string()))?,
                ),
            )])
            .map_err(|e| ManyError::unknown(e.to_string()))?;
        Ok(())
    }

    pub fn commit_multisig_transaction(
        &mut self,
        tx_id: &[u8],
        tx: &TransactionStorage,
    ) -> Result<(), ManyError> {
        self.persistent_store
            .apply(&[(
                key_for_multisig_transaction(tx_id),
                fmerk::Op::Put(
                    minicbor::to_vec(tx)
                        .map_err(|e| ManyError::serialization_error(e.to_string()))?,
                ),
            )])
            .unwrap();
        if !self.blockchain {
            self.persistent_store
                .commit(&[])
                .expect("Could not commit to store.");
        }
        Ok(())
    }

    pub fn create_multisig_transaction(
        &mut self,
        sender: &Identity,
        arg: account::features::multisig::SubmitTransactionArg,
    ) -> Result<Vec<u8>, ManyError> {
        let tx_id = self.new_transaction_id();
        let key = key_for_multisig_transaction(tx_id.0.as_slice());

        let account_id = arg
            .account
            .or_else(|| match &arg.transaction {
                TransactionInfo::Send { from, .. } => Some(*from),
                _ => None,
            })
            .ok_or_else(|| {
                ManyError::unknown(
                    "Could not find an account to initiate the transaction.".to_string(),
                )
            })?;

        // Validate the transaction's information.
        match &arg.transaction {
            TransactionInfo::Send { from, .. } => {
                if !from.matches(&account_id) {
                    return Err(ManyError::unknown("Invalid transaction.".to_string()));
                }
            }
            _ => {}
        }

        let account = self
            .get_account(&account_id)
            .ok_or_else(|| account::errors::unknown_account(account_id))?;

        let is_owner = account.has_role(sender, "owner");
        if !(is_owner || account.has_role(sender, "canMultisigSubmit")) {
            return Err(account::errors::user_needs_role("canMultisigSubmit"));
        }
        let multisig_f = account
            .features
            .get::<account::features::multisig::MultisigAccountFeature>()?;

        let threshold = match arg.threshold {
            Some(t) if is_owner => t,
            Some(_) => return Err(account::errors::user_needs_role("owner")),
            _ => multisig_f
                .arg
                .threshold
                .unwrap_or(MULTISIG_DEFAULT_THRESHOLD),
        };
        let timeout_in_secs = match arg.timeout_in_secs {
            Some(t) if is_owner => t,
            Some(_) => return Err(account::errors::user_needs_role("owner")),
            _ => multisig_f
                .arg
                .timeout_in_secs
                .unwrap_or(MULTISIG_DEFAULT_TIMEOUT_IN_SECS),
        }
        .min(MULTISIG_MAXIMUM_TIMEOUT_IN_SECS);
        let execute_automatically = match arg.execute_automatically {
            Some(e) if is_owner => e,
            Some(_) => return Err(account::errors::user_needs_role("owner")),
            _ => multisig_f
                .arg
                .execute_automatically
                .unwrap_or(MULTISIG_DEFAULT_EXECUTE_AUTOMATICALLY),
        };
        let time = self.current_time.unwrap_or_else(SystemTime::now);

        // Calculate the approver list, set their approvals to false, except for
        // the sender.
        let approvers = BTreeMap::new();

        let storage = TransactionStorage {
            memo: arg.memo,
            transaction: arg.transaction,
            submitter: *sender,
            approvers,
            threshold,
            execute_automatically,
            timeout: Timestamp(time + Duration::from_secs(timeout_in_secs)),
        };

        self.commit_multisig_transaction(tx_id.0.as_slice(), &storage)?;

        Ok(tx_id.0.to_vec())
    }

    pub fn get_multisig_info(&self, tx_id: &[u8]) -> Result<TransactionStorage, ManyError> {
        let storage_bytes = self
            .persistent_store
            .get(&key_for_multisig_transaction(tx_id))
            .unwrap_or(None)
            .ok_or_else(|| account::features::multisig::errors::transaction_cannot_be_found())?;
        minicbor::decode::<TransactionStorage>(&storage_bytes)
            .map_err(|e| ManyError::deserialization_error(e.to_string()))
    }

    pub fn approve_multisig(
        &mut self,
        sender: &Identity,
        account: &Identity,
        tx_id: &[u8],
    ) -> Result<bool, ManyError> {
        // Verify the sender has the rights to the account.
        let account = self
            .get_account(account)
            .ok_or_else(|| account::errors::unknown_account(account.to_string()))?;

        if !account.has_role(sender, "canMultisigApprove") {
            return Err(account::errors::user_needs_role("canMultisigApprove"));
        }

        let mut storage = self.get_multisig_info(tx_id)?;

        if let Some(info) = storage.approvers.get_mut(sender) {
            info.approved = true;
        } else {
            return Err(account::features::multisig::errors::user_cannot_approve_transaction());
        }

        self.commit_multisig_transaction(tx_id, &storage)?;

        // If the transaction executes automatically, calculate number of approvers.
        if storage.execute_automatically
            && storage.approvers.values().filter(|i| i.approved).count()
                >= storage.threshold as usize
        {
            self.execute_multisig_transaction_internal(tx_id, &storage)?;
            return Ok(true);
        }

        Ok(false)
    }

    pub fn execute_multisig(
        &mut self,
        sender: &Identity,
        account: &Identity,
        tx_id: &[u8],
    ) -> Result<(), ManyError> {
        let mut storage = self.get_multisig_info(tx_id)?;

        // Verify the sender has the rights to the account.
        let account = self
            .get_account(account)
            .ok_or_else(|| account::errors::unknown_account(account.to_string()))?;

        if !(account.has_role(sender, "owner") || storage.submitter.matches(sender)) {
            return Err(account::errors::user_needs_role("canMultisigApprove"));
        }

        if storage.approvers.values().filter(|i| i.approved).count() >= storage.threshold as usize {
            self.execute_multisig_transaction_internal(tx_id, &storage)?;
        }

        Ok(())
    }

    fn delete_multisig_transaction(&mut self, tx_id: &[u8]) -> Result<(), ManyError> {
        self.persistent_store
            .apply(&[(key_for_multisig_transaction(tx_id), fmerk::Op::Delete)])
            .unwrap();
        if !self.blockchain {
            self.persistent_store
                .commit(&[])
                .expect("Could not commit to store.");
        }
        Ok(())
    }

    fn execute_multisig_transaction_internal(
        &mut self,
        tx_id: &[u8],
        storage: &TransactionStorage,
    ) -> Result<ResponseMessage, ManyError> {
        match &storage.transaction {
            TransactionInfo::Send {
                from,
                to,
                symbol,
                amount,
            } => {
                self.delete_multisig_transaction(tx_id)?;
                self.send(from, to, symbol, amount.clone())?;

                Ok(ResponseMessage {
                    from: *from,
                    to: None,
                    ..Default::default()
                })
            }
            _ => Err(account::features::multisig::errors::transaction_type_unsupported()),
        }
    }
}

pub struct LedgerIterator<'a> {
    inner: fmerk::rocksdb::DBIterator<'a>,
}

impl<'a> LedgerIterator<'a> {
    pub fn scoped_by_id(
        merk: &'a fmerk::Merk,
        range: CborRange<TransactionId>,
        order: SortOrder,
    ) -> Self {
        use fmerk::rocksdb::{IteratorMode, ReadOptions};
        let mut opts = ReadOptions::default();

        match range.start_bound() {
            Bound::Included(x) => opts.set_iterate_lower_bound(key_for_transaction(x.clone())),
            Bound::Excluded(x) => opts.set_iterate_lower_bound(key_for_transaction(x.clone() + 1)),
            Bound::Unbounded => opts.set_iterate_lower_bound(TRANSACTIONS_ROOT),
        }
        match range.end_bound() {
            Bound::Included(x) => opts.set_iterate_upper_bound(key_for_transaction(x.clone() + 1)),
            Bound::Excluded(x) => opts.set_iterate_upper_bound(key_for_transaction(x.clone())),
            Bound::Unbounded => {
                let mut bound = TRANSACTIONS_ROOT.to_vec();
                bound[TRANSACTIONS_ROOT.len() - 1] += 1;
                opts.set_iterate_upper_bound(bound);
            }
        }

        let mode = match order {
            SortOrder::Indeterminate | SortOrder::Ascending => IteratorMode::Start,
            SortOrder::Descending => IteratorMode::End,
        };

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
