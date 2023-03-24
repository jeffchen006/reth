use cita_trie::{FixedHasherKeccak as HasherKeccak, PatriciaTrie, Trie};
use parking_lot::Mutex;
use reth_codecs::Compact;
use reth_db::{
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW},
    models::{AccountBeforeTx, TransitionIdAddress},
    tables,
    transaction::{DbTx, DbTxGAT, DbTxMut, DbTxMutGAT},
};
use reth_primitives::{
    hex_literal::hex, keccak256, proofs::EMPTY_ROOT, Account, Address, ProofCheckpoint,
    StorageEntry, StorageTrieEntry, TransitionId, H256, KECCAK_EMPTY, U256,
};
use reth_rlp::{
    encode_fixed_size, Decodable, DecodeError, Encodable, RlpDecodable, RlpEncodable,
    EMPTY_STRING_CODE,
};
use reth_tracing::tracing::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    marker::PhantomData,
    ops::Range,
    sync::Arc,
};

/// Merkle Trie error types
#[allow(missing_docs)]
#[derive(Debug, thiserror::Error)]
pub enum TrieError {
    /// Error returned by the underlying implementation.
    #[error("Some error occurred: {0}")]
    InternalError(#[from] cita_trie::TrieError),
    /// The database doesn't contain the root of the trie.
    #[error("The root node wasn't found in the DB")]
    MissingAccountRoot(H256),
    #[error("The storage root node wasn't found in the DB")]
    MissingStorageRoot(H256),
    /// Error returned by the database.
    #[error("{0:?}")]
    DatabaseError(#[from] reth_db::Error),
    /// Error when encoding/decoding a value.
    #[error("{0:?}")]
    DecodeError(#[from] DecodeError),
    #[error("Trie requires committing a checkpoint.")]
    UnexpectedCheckpoint,
}

type AccountsTrieCursor<'tx, TX> =
    Arc<Mutex<<TX as DbTxMutGAT<'tx>>::CursorMut<tables::AccountsTrie>>>;

/// Database wrapper implementing HashDB trait, with a read-write transaction.
pub struct HashDatabaseMut<'tx, TX: DbTxMutGAT<'tx>> {
    accounts_trie_cursor: AccountsTrieCursor<'tx, TX>,
}

impl<'tx, 'db, TX> cita_trie::DB for HashDatabaseMut<'tx, TX>
where
    TX: DbTxMut<'db> + DbTx<'db> + Send + Sync,
{
    type Error = TrieError;

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self.accounts_trie_cursor.lock().seek_exact(H256::from_slice(key))?.map(|(_, v)| v))
    }

    fn contains(&self, key: &[u8]) -> Result<bool, Self::Error> {
        Ok(<Self as cita_trie::DB>::get(self, key)?.is_some())
    }

    fn insert(&self, _key: Vec<u8>, _value: Vec<u8>) -> Result<(), Self::Error> {
        unreachable!("Use batch instead.");
    }

    /// Insert a batch of data into the cache.
    fn insert_batch(&self, _keys: Vec<Vec<u8>>, _values: Vec<Vec<u8>>) -> Result<(), Self::Error> {
        unreachable!("Use map instead.");
    }

    /// Insert a map of data into the cache.
    fn insert_map(&self, mut kv: Vec<([u8; 32], Vec<u8>)>) -> Result<(), Self::Error> {
        kv.sort_by(|a, b| a.0.cmp(&b.0));
        let mut cursor = self.accounts_trie_cursor.lock();

        for (key, value) in kv.into_iter() {
            cursor.upsert(H256::from(key), value)?;
        }
        Ok(())
    }

    fn remove_batch(&self, keys: &[[u8; 32]]) -> Result<(), Self::Error> {
        let mut cursor = self.accounts_trie_cursor.lock();
        for key in keys {
            if cursor.seek_exact(H256::from(key))?.is_some() {
                cursor.delete_current()?;
            }
        }
        Ok(())
    }

    fn remove(&self, _key: &[u8]) -> Result<(), Self::Error> {
        unreachable!("Use batch instead.");
    }

    fn flush(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<'tx, 'db, TX> HashDatabaseMut<'tx, TX>
where
    TX: DbTxMut<'db> + DbTx<'db> + Send + Sync,
{
    /// Instantiates a new Database for the accounts trie, with an empty root
    pub fn new(tx: &'tx TX) -> Result<Self, TrieError> {
        let mut accounts_trie_cursor = tx.cursor_write::<tables::AccountsTrie>()?;

        let root = EMPTY_ROOT;
        if accounts_trie_cursor.seek_exact(root)?.is_none() {
            accounts_trie_cursor.upsert(root, [EMPTY_STRING_CODE].to_vec())?;
        }

        Ok(Self { accounts_trie_cursor: Arc::new(Mutex::new(accounts_trie_cursor)) })
    }

    /// Instantiates a new Database for the accounts trie, with an existing root
    pub fn from_root(tx: &'tx TX, root: H256) -> Result<Self, TrieError> {
        let mut accounts_trie_cursor = tx.cursor_write::<tables::AccountsTrie>()?;

        if root == EMPTY_ROOT {
            return Self::new(tx)
        }
        accounts_trie_cursor.seek_exact(root)?.ok_or(TrieError::MissingAccountRoot(root))?;

        Ok(Self { accounts_trie_cursor: Arc::new(Mutex::new(accounts_trie_cursor)) })
    }
}

type StoragesTrieCursor<'tx, TX> =
    Arc<Mutex<<TX as DbTxMutGAT<'tx>>::DupCursorMut<tables::StoragesTrie>>>;

/// Database wrapper implementing HashDB trait, with a read-write transaction.
pub struct DupHashDatabaseMut<'tx, TX: DbTxMutGAT<'tx>> {
    storages_trie_cursor: StoragesTrieCursor<'tx, TX>,
    key: H256,
    is_update: bool,
}

impl<'tx, 'db, TX> cita_trie::DB for DupHashDatabaseMut<'tx, TX>
where
    TX: DbTxMut<'db> + DbTx<'db> + Send + Sync,
{
    type Error = TrieError;

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        let subkey = H256::from_slice(key);
        Ok(self
            .storages_trie_cursor
            .lock()
            .seek_by_key_subkey(self.key, subkey)?
            .filter(|entry| entry.hash == subkey)
            .map(|entry| entry.node))
    }

    fn contains(&self, key: &[u8]) -> Result<bool, Self::Error> {
        Ok(<Self as cita_trie::DB>::get(self, key)?.is_some())
    }

    fn insert(&self, _key: Vec<u8>, _value: Vec<u8>) -> Result<(), Self::Error> {
        unreachable!("Use batch instead.");
    }

    /// Insert a batch of data into the cache.
    fn insert_batch(&self, _keys: Vec<Vec<u8>>, _values: Vec<Vec<u8>>) -> Result<(), Self::Error> {
        unreachable!("Use map instead.");
    }

    /// Insert a map of data into the cache.
    fn insert_map(&self, mut kv: Vec<([u8; 32], Vec<u8>)>) -> Result<(), Self::Error> {
        kv.sort_by(|a, b| a.0.cmp(&b.0));
        let mut cursor = self.storages_trie_cursor.lock();

        for (key, node) in kv.into_iter() {
            let hash = H256::from(key);

            if hash == EMPTY_ROOT {
                continue
            }

            if self.is_update {
                if cursor.seek_by_key_subkey(self.key, hash)?.filter(|e| e.hash == hash).is_some() {
                    cursor.delete_current()?;
                }
                cursor.upsert(self.key, StorageTrieEntry { hash, node })?;
            } else {
                cursor.append_dup(self.key, StorageTrieEntry { hash, node })?;
            }
        }
        Ok(())
    }

    fn remove_batch(&self, keys: &[[u8; 32]]) -> Result<(), Self::Error> {
        let mut cursor = self.storages_trie_cursor.lock();
        for key in keys {
            let hash = H256::from(key);

            if hash == EMPTY_ROOT {
                continue
            }

            if cursor.seek_by_key_subkey(self.key, hash)?.filter(|e| e.hash == hash).is_some() {
                cursor.delete_current()?;
            }
        }
        Ok(())
    }

    fn remove(&self, _key: &[u8]) -> Result<(), Self::Error> {
        unreachable!("Use batch instead.");
    }

    fn flush(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<'tx, 'db, TX> DupHashDatabaseMut<'tx, TX>
where
    TX: DbTxMut<'db> + DbTx<'db> + Send + Sync,
{
    /// Instantiates a new Database for the storage trie, with an empty root
    pub fn new(
        storages_trie_cursor: StoragesTrieCursor<'tx, TX>,
        key: H256,
    ) -> Result<Self, TrieError> {
        Ok(Self { storages_trie_cursor, key, is_update: false })
    }

    /// Instantiates a new Database for the storage trie, with an existing root
    pub fn from_root(
        storages_trie_cursor: StoragesTrieCursor<'tx, TX>,
        key: H256,
        root: H256,
    ) -> Result<Self, TrieError> {
        if root == EMPTY_ROOT {
            return Self::new(storages_trie_cursor, key)
        }
        storages_trie_cursor
            .lock()
            .seek_by_key_subkey(key, root)?
            .filter(|entry| entry.hash == root)
            .ok_or(TrieError::MissingStorageRoot(root))?;
        Ok(Self { storages_trie_cursor, key, is_update: true })
    }
}

/// Database wrapper implementing HashDB trait, with a read-only transaction.
pub struct HashDatabase<'tx, 'itx, TX: DbTx<'itx>> {
    tx: &'tx TX,
    _p: PhantomData<&'itx ()>, // to suppress "unused" lifetime 'itx
}

impl<'tx, 'itx, TX: DbTx<'itx>> HashDatabase<'tx, 'itx, TX> {
    /// Creates a new Hash database with the given transaction
    pub fn new(tx: &'tx TX) -> Self {
        Self { tx, _p: Default::default() }
    }
}

impl<'tx, 'itx, TX> cita_trie::DB for HashDatabase<'tx, 'itx, TX>
where
    TX: DbTx<'itx>,
{
    type Error = TrieError;

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        Ok(self.tx.get::<tables::AccountsTrie>(H256::from_slice(key))?)
    }

    fn contains(&self, key: &[u8]) -> Result<bool, Self::Error> {
        Ok(<Self as cita_trie::DB>::get(self, key)?.is_some())
    }

    fn insert(&self, _key: Vec<u8>, _value: Vec<u8>) -> Result<(), Self::Error> {
        // this could be avoided if cita_trie::DB was split into two traits
        // with read and write operations respectively
        unimplemented!("insert isn't valid for read-only transaction");
    }

    fn remove(&self, _key: &[u8]) -> Result<(), Self::Error> {
        unimplemented!("remove isn't valid for read-only transaction");
    }

    fn flush(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<'tx, 'itx, TX: DbTx<'itx>> HashDatabase<'tx, 'itx, TX> {
    /// Instantiates a new Database for the accounts trie, with an existing root
    fn from_root(tx: &'tx TX, root: H256) -> Result<Self, TrieError> {
        tx.get::<tables::AccountsTrie>(root)?.ok_or(TrieError::MissingAccountRoot(root))?;
        Ok(Self { tx, _p: Default::default() })
    }
}

/// Database wrapper implementing HashDB trait, with a read-only transaction.
pub struct DupHashDatabase<'tx, 'itx, TX: DbTx<'itx>> {
    tx: &'tx TX,
    key: H256,
    _p: PhantomData<&'itx ()>, // to suppress "unused" lifetime 'itx
}

impl<'tx, 'itx, TX: DbTx<'itx>> DupHashDatabase<'tx, 'itx, TX> {
    /// Creates a new DupHash database with the given transaction and key.
    pub fn new(tx: &'tx TX, key: H256) -> Self {
        Self { tx, key, _p: Default::default() }
    }
}

impl<'tx, 'itx, TX> cita_trie::DB for DupHashDatabase<'tx, 'itx, TX>
where
    TX: DbTx<'itx>,
{
    type Error = TrieError;

    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, Self::Error> {
        let mut cursor = self.tx.cursor_dup_read::<tables::StoragesTrie>()?;
        Ok(cursor.seek_by_key_subkey(self.key, H256::from_slice(key))?.map(|entry| entry.node))
    }

    fn contains(&self, key: &[u8]) -> Result<bool, Self::Error> {
        Ok(<Self as cita_trie::DB>::get(self, key)?.is_some())
    }

    fn insert(&self, _key: Vec<u8>, _value: Vec<u8>) -> Result<(), Self::Error> {
        // Caching and bulk inserting shouldn't be needed, as the data is ordered
        unimplemented!("insert isn't valid for read-only transaction");
    }

    fn remove(&self, _key: &[u8]) -> Result<(), Self::Error> {
        unimplemented!("remove isn't valid for read-only transaction");
    }

    fn flush(&self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<'tx, 'itx, TX: DbTx<'itx>> DupHashDatabase<'tx, 'itx, TX> {
    /// Instantiates a new Database for the storage trie, with an existing root
    fn from_root(tx: &'tx TX, key: H256, root: H256) -> Result<Self, TrieError> {
        tx.cursor_dup_read::<tables::StoragesTrie>()?
            .seek_by_key_subkey(key, root)?
            .ok_or(TrieError::MissingAccountRoot(root))?;
        Ok(Self { tx, key, _p: Default::default() })
    }
}

/// An Ethereum account, for RLP encoding traits deriving.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, RlpEncodable, RlpDecodable)]
pub struct EthAccount {
    /// Account nonce.
    nonce: u64,
    /// Account balance.
    balance: U256,
    /// Account's storage root.
    storage_root: H256,
    /// Hash of the account's bytecode.
    code_hash: H256,
}

impl From<Account> for EthAccount {
    fn from(acc: Account) -> Self {
        EthAccount {
            nonce: acc.nonce,
            balance: acc.balance,
            storage_root: EMPTY_ROOT,
            code_hash: acc.bytecode_hash.unwrap_or(KECCAK_EMPTY),
        }
    }
}

impl EthAccount {
    /// Set storage root on account.
    pub fn with_storage_root(mut self, storage_root: H256) -> Self {
        self.storage_root = storage_root;
        self
    }

    /// Get account's storage root.
    pub fn storage_root(&self) -> H256 {
        self.storage_root
    }
}

/// A merkle proof of existence (or nonexistence) of a leaf value. Consists
/// of a the encoded nodes in the path from the root of the tree to the leaf.
pub type MerkleProof = Vec<Vec<u8>>;

/// Struct for calculating the root of a merkle patricia tree,
/// while populating the database with intermediate hashes.
#[derive(Debug)]
pub struct DBTrieLoader<'tx, TX> {
    /// The maximum number of keys to insert before committing. Both from `AccountsTrie` and
    /// `StoragesTrie`.
    pub commit_threshold: u64,
    /// The current number of inserted keys from both `AccountsTrie` and `StoragesTrie`.
    pub current: u64,
    /// The transaction to use for inserting the trie nodes.
    pub tx: &'tx TX,
}

/// Status of the trie calculation.
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum TrieProgress {
    /// Trie has finished with the passed root.
    Complete(H256),
    /// Trie has hit its commit threshold.
    InProgress(ProofCheckpoint),
}

impl TrieProgress {
    /// Consumes the root from its `Complete` variant. If that's not possible, throw
    /// `TrieError::UnexpectedCheckpoint`.
    pub fn root(self) -> Result<H256, TrieError> {
        match self {
            Self::Complete(root) => Ok(root),
            _ => Err(TrieError::UnexpectedCheckpoint),
        }
    }
}

impl<'tx, TX> DBTrieLoader<'tx, TX> {
    /// Create new instance of trie loader.
    pub fn new(tx: &'tx TX) -> Self {
        Self { tx, commit_threshold: 2_000_000, current: 0 }
    }

    /// Create new instance of trie loader with a specific threshold.
    pub fn new_with_threshold(tx: &'tx TX, commit_threshold: u64) -> Self {
        Self { tx, commit_threshold, current: 0 }
    }
}

// Read-write impls
impl<'tx, 'db, TX> DBTrieLoader<'tx, TX>
where
    TX: DbTxMut<'db> + DbTx<'db> + Send + Sync,
{
    /// Calculates the root of the state trie, saving intermediate hashes in the database.
    pub fn calculate_root(&mut self) -> Result<TrieProgress, TrieError> {
        let mut checkpoint = self.get_checkpoint()?;

        if checkpoint.hashed_address.is_none() {
            self.tx.clear::<tables::AccountsTrie>()?;
            self.tx.clear::<tables::StoragesTrie>()?;
        }
        let previous_root = checkpoint.account_root.unwrap_or(EMPTY_ROOT);

        let hasher = Arc::new(HasherKeccak::new());
        let mut trie = if let Some(root) = checkpoint.account_root {
            PatriciaTrie::from(
                Arc::new(HashDatabaseMut::from_root(self.tx, root)?),
                hasher,
                root.as_bytes(),
            )?
        } else {
            PatriciaTrie::new(Arc::new(HashDatabaseMut::new(self.tx)?), hasher)
        };

        let mut accounts_cursor = self.tx.cursor_read::<tables::HashedAccount>()?;
        let mut storage_cursor = self.tx.cursor_dup_read::<tables::HashedStorage>()?;

        let storage_trie_cursor =
            Arc::new(Mutex::new(self.tx.cursor_dup_write::<tables::StoragesTrie>()?));

        let first_hashed_address = checkpoint.hashed_address.take();
        let mut walker = accounts_cursor.walk(first_hashed_address)?;
        if first_hashed_address.is_some() && checkpoint.storage_root.is_none() {
            walker.next();
        }

        while let Some((hashed_address, account)) = walker.next().transpose()? {
            match self.calculate_storage_root(
                hashed_address,
                &mut storage_cursor,
                storage_trie_cursor.clone(),
                checkpoint.storage_key.take(),
                checkpoint.storage_root.take(),
            )? {
                TrieProgress::Complete(root) => {
                    let value = EthAccount::from(account).with_storage_root(root);

                    let mut out = Vec::new();
                    Encodable::encode(&value, &mut out);
                    trie.insert(hashed_address.as_bytes(), out)?;

                    if self.has_hit_threshold() {
                        return self.save_account_checkpoint(
                            ProofCheckpoint::default(),
                            self.replace_account_root(trie, previous_root)?,
                            hashed_address,
                        )
                    }
                }
                TrieProgress::InProgress(checkpoint) => {
                    return self.save_account_checkpoint(
                        checkpoint,
                        self.replace_account_root(trie, previous_root)?,
                        hashed_address,
                    )
                }
            }
        }

        // Reset inner stage progress
        self.save_checkpoint(ProofCheckpoint::default())?;

        Ok(TrieProgress::Complete(self.replace_account_root(trie, previous_root)?))
    }

    fn calculate_storage_root(
        &mut self,
        address: H256,
        storage_cursor: &mut <TX as DbTxGAT<'tx>>::DupCursor<tables::HashedStorage>,
        storage_trie_cursor: StoragesTrieCursor<'tx, TX>,
        next_storage: Option<H256>,
        previous_root: Option<H256>,
    ) -> Result<TrieProgress, TrieError> {
        let hasher = Arc::new(HasherKeccak::new());
        let previous_root = previous_root.unwrap_or(EMPTY_ROOT);

        let has_checkpoint = next_storage.is_some();
        let mut current_entry = if let Some(entry) = next_storage {
            storage_cursor.seek_by_key_subkey(address, entry)?.filter(|e| e.key == entry)
        } else {
            storage_cursor.seek_by_key_subkey(address, H256::zero())?
        };

        if current_entry.is_none() {
            return Ok(TrieProgress::Complete(self.replace_storage_root(
                EMPTY_ROOT,
                storage_trie_cursor,
                address,
                previous_root,
            )?))
        }

        let mut trie = if has_checkpoint {
            PatriciaTrie::from(
                Arc::new(DupHashDatabaseMut::<TX>::from_root(
                    storage_trie_cursor.clone(),
                    address,
                    previous_root,
                )?),
                hasher,
                previous_root.as_bytes(),
            )?
        } else {
            PatriciaTrie::new_with_hash(
                Arc::new(DupHashDatabaseMut::<TX>::new(storage_trie_cursor.clone(), address)?),
                hasher,
                EMPTY_ROOT.as_slice().to_vec(),
            )
        };

        while let Some(StorageEntry { key: storage_key, value }) = current_entry {
            let out = encode_fixed_size(&value).to_vec();
            trie.insert(&storage_key.to_fixed_bytes(), out)?;
            // Should be able to use walk_dup, but any call to next() causes an assert fail in
            // mdbx.c
            current_entry = storage_cursor.next_dup()?.map(|(_, v)| v);
            let threshold = self.has_hit_threshold();
            if let Some(current_entry) = current_entry {
                if threshold {
                    return Ok(TrieProgress::InProgress(ProofCheckpoint {
                        storage_root: Some(self.replace_storage_root(
                            H256::from_slice(trie.root()?.as_slice()),
                            storage_trie_cursor,
                            address,
                            previous_root,
                        )?),
                        storage_key: Some(current_entry.key),
                        ..Default::default()
                    }))
                }
            }
        }

        Ok(TrieProgress::Complete(self.replace_storage_root(
            H256::from_slice(trie.root()?.as_slice()),
            storage_trie_cursor,
            address,
            previous_root,
        )?))
    }

    /// Calculates the root of the state trie by updating an existing trie.
    pub fn update_root(
        &mut self,
        mut previous_root: H256,
        tid_range: Range<TransitionId>,
    ) -> Result<TrieProgress, TrieError> {
        let mut checkpoint = self.get_checkpoint()?;

        if let Some(account_root) = checkpoint.account_root.take() {
            previous_root = account_root;
        }

        let next_acc = checkpoint.hashed_address.take();
        let changed_accounts: BTreeMap<_, _> = self
            .gather_changes(tid_range)?
            .into_iter()
            .skip_while(|(addr, _)| next_acc.is_some() && next_acc.expect("is some") != *addr)
            .collect();

        let mut trie = PatriciaTrie::from(
            Arc::new(HashDatabaseMut::from_root(self.tx, previous_root)?),
            Arc::new(HasherKeccak::new()),
            previous_root.as_bytes(),
        )?;

        let mut accounts_cursor = self.tx.cursor_read::<tables::HashedAccount>()?;
        let mut storage_cursor = self.tx.cursor_dup_read::<tables::HashedStorage>()?;

        let storage_trie_cursor =
            Arc::new(Mutex::new(self.tx.cursor_dup_write::<tables::StoragesTrie>()?));

        let number_of_changed_accounts = changed_accounts.len();
        for (idx, (hashed_address, changed_storages)) in changed_accounts.into_iter().enumerate() {
            println!("Entering with account {:?}", hashed_address);
            let res = if let Some(account) = trie.get(hashed_address.as_slice())? {
                println!("Incrementally calculating storage root");
                // NOTE: We need to remove the account here, because on some paths it is not
                // re-inserted, leading to us hitting the second branch after certain checkpoints
                // trie.remove(hashed_address.as_bytes())?;

                let storage_root = EthAccount::decode(&mut account.as_slice())?.storage_root;
                self.update_storage_root(
                    checkpoint.storage_root.take().unwrap_or(storage_root),
                    hashed_address,
                    storage_trie_cursor.clone(),
                    changed_storages,
                    checkpoint.storage_key.take(),
                )?
            } else {
                println!("Calculating storage root from scratch");
                self.calculate_storage_root(
                    hashed_address,
                    &mut storage_cursor,
                    storage_trie_cursor.clone(),
                    checkpoint.storage_key.take(),
                    checkpoint.storage_root.take(),
                )?
            };

            let storage_root = match res {
                TrieProgress::Complete(root) => root,
                TrieProgress::InProgress(checkpoint) => {
                    return self.save_account_checkpoint(
                        checkpoint,
                        self.replace_account_root(trie, previous_root)?,
                        hashed_address,
                    )
                }
            };

            if let Some((_, account)) = accounts_cursor.seek_exact(hashed_address)? {
                let value = EthAccount::from(account).with_storage_root(storage_root);

                let mut out = Vec::new();
                Encodable::encode(&value, &mut out);

                trie.insert(hashed_address.as_bytes(), out)?;

                // check if done *before* we check the threshold
                if self.has_hit_threshold() && idx != number_of_changed_accounts - 1 {
                    return self.save_account_checkpoint(
                        ProofCheckpoint::default(),
                        self.replace_account_root(trie, previous_root)?,
                        hashed_address,
                    )
                }
            }
        }

        // Reset inner stage progress
        self.save_checkpoint(ProofCheckpoint::default())?;

        Ok(TrieProgress::Complete(self.replace_account_root(trie, previous_root)?))
    }

    /// Update the account's storage root
    fn update_storage_root(
        &mut self,
        previous_root: H256,
        address: H256,
        storage_trie_cursor: StoragesTrieCursor<'tx, TX>,
        changed_storages: BTreeSet<H256>,
        next_storage: Option<H256>,
    ) -> Result<TrieProgress, TrieError> {
        let mut hashed_storage_cursor = self.tx.cursor_dup_read::<tables::HashedStorage>()?;
        if hashed_storage_cursor.seek_by_key_subkey(address, H256::zero())?.is_none() {
            return Ok(TrieProgress::Complete(self.replace_storage_root(
                EMPTY_ROOT,
                storage_trie_cursor,
                address,
                previous_root,
            )?))
        }

        // NOTE(onbjerg): Will fail if account is new
        // NOTE: We have to load with the previous storage root, otherwise all nodes in the trie we
        // computed in the last run (where we checkpointed) are lost
        let mut trie = PatriciaTrie::from(
            Arc::new(DupHashDatabaseMut::<TX>::from_root(
                storage_trie_cursor.clone(),
                address,
                previous_root,
            )?),
            Arc::new(HasherKeccak::new()),
            previous_root.as_bytes(),
        )?;

        let changed_storages: BTreeSet<_> = changed_storages
            .into_iter()
            // We know the keys are sorted, so any key less than or equal to the checkpoint should
            // be skipped (since the checkpoint is the last storage slot we have processed)
            .skip_while(|k| next_storage.is_some() && *k <= next_storage.expect("is some"))
            .collect();
        let num_changed_storages = changed_storages.len();
        for (idx, key) in changed_storages.into_iter().enumerate() {
            if let Some(StorageEntry { value, .. }) =
                hashed_storage_cursor.seek_by_key_subkey(address, key)?.filter(|e| e.key == key)
            {
                let out = encode_fixed_size(&value).to_vec();
                trie.insert(key.as_bytes(), out)?;

                if self.has_hit_threshold() && idx != num_changed_storages - 1 {
                    return Ok(TrieProgress::InProgress(ProofCheckpoint {
                        storage_root: Some(self.replace_storage_root(
                            H256::from_slice(trie.root()?.as_slice()),
                            storage_trie_cursor,
                            address,
                            previous_root,
                        )?),
                        storage_key: Some(key),
                        ..Default::default()
                    }))
                }
            } else {
                trie.remove(key.as_bytes())?;
            }
        }

        Ok(TrieProgress::Complete(self.replace_storage_root(
            H256::from_slice(trie.root()?.as_slice()),
            storage_trie_cursor,
            address,
            previous_root,
        )?))
    }

    fn gather_changes(
        &self,
        tid_range: Range<TransitionId>,
    ) -> Result<BTreeMap<H256, BTreeSet<H256>>, TrieError> {
        let mut account_cursor = self.tx.cursor_read::<tables::AccountChangeSet>()?;

        let mut account_changes: BTreeMap<Address, BTreeSet<H256>> = BTreeMap::new();

        let mut walker = account_cursor.walk_range(tid_range.clone())?;

        while let Some((_, AccountBeforeTx { address, .. })) = walker.next().transpose()? {
            account_changes.insert(address, Default::default());
        }

        let mut storage_cursor = self.tx.cursor_dup_read::<tables::StorageChangeSet>()?;

        let start = TransitionIdAddress((tid_range.start, Address::zero()));
        let end = TransitionIdAddress((tid_range.end, Address::zero()));
        let mut walker = storage_cursor.walk_range(start..end)?;

        while let Some((TransitionIdAddress((_, address)), StorageEntry { key, .. })) =
            walker.next().transpose()?
        {
            account_changes.entry(address).or_default().insert(key);
        }

        let hashed_changes = account_changes
            .into_iter()
            .map(|(address, storage)| {
                (keccak256(address), storage.into_iter().map(keccak256).collect())
            })
            .collect();

        Ok(hashed_changes)
    }

    fn save_account_checkpoint(
        &mut self,
        mut checkpoint: ProofCheckpoint,
        root: H256,
        hashed_address: H256,
    ) -> Result<TrieProgress, TrieError> {
        checkpoint.account_root = Some(root);
        checkpoint.hashed_address = Some(hashed_address);

        debug!(target: "sync::stages::merkle::exec", account = ?hashed_address, storage = ?checkpoint.storage_key, "Saving inner trie checkpoint");

        self.save_checkpoint(checkpoint)?;

        Ok(TrieProgress::InProgress(checkpoint))
    }

    fn has_hit_threshold(&mut self) -> bool {
        self.current += 1;
        self.current >= self.commit_threshold
    }

    /// Saves the trie progress
    pub fn save_checkpoint(&mut self, checkpoint: ProofCheckpoint) -> Result<(), TrieError> {
        let mut buf = vec![];
        checkpoint.to_compact(&mut buf);

        // It allows unwind (which commits), to reuse this instance.
        self.current = 0;

        Ok(self.tx.put::<tables::SyncStageProgress>("TrieLoader".into(), buf)?)
    }

    /// Gets the trie progress
    pub fn get_checkpoint(&self) -> Result<ProofCheckpoint, TrieError> {
        let buf =
            self.tx.get::<tables::SyncStageProgress>("TrieLoader".into())?.unwrap_or_default();

        if buf.is_empty() {
            return Ok(ProofCheckpoint::default())
        }

        let (checkpoint, _) = ProofCheckpoint::from_compact(&buf, buf.len());

        if checkpoint.account_root.is_some() {
            debug!(target: "sync::stages::merkle::exec", checkpoint = ?checkpoint, "Continuing inner trie checkpoint");
        }

        Ok(checkpoint)
    }

    /// Finds the most recent account trie root and removes the previous one if applicable.
    fn replace_account_root(
        &self,
        mut trie: PatriciaTrie<HashDatabaseMut<'_, TX>, HasherKeccak>,
        previous_root: H256,
    ) -> Result<H256, TrieError> {
        let new_root = H256::from_slice(trie.root()?.as_slice());

        if new_root != previous_root {
            let mut cursor = self.tx.cursor_write::<tables::AccountsTrie>()?;
            if cursor.seek_exact(previous_root)?.is_some() {
                cursor.delete_current()?;
            }
        }

        Ok(new_root)
    }

    /// Finds the most recent storage trie root and removes the previous one if applicable.
    fn replace_storage_root(
        &self,
        new_root: H256,
        storage_trie_cursor: StoragesTrieCursor<'tx, TX>,
        address: H256,
        previous_root: H256,
    ) -> Result<H256, TrieError> {
        if new_root != previous_root && previous_root != EMPTY_ROOT {
            let mut trie_cursor = storage_trie_cursor.lock();
            if trie_cursor
                .seek_by_key_subkey(address, previous_root)?
                .filter(|entry| entry.hash == previous_root)
                .is_some()
            {
                trie_cursor.delete_current()?;
            }
        }

        Ok(new_root)
    }
}

// Read-only impls
impl<'tx, 'db, TX> DBTrieLoader<'tx, TX>
where
    TX: DbTx<'db> + Send + Sync,
{
    /// Returns a Merkle proof of the given account, plus its storage root hash.
    pub fn generate_acount_proof(
        &self,
        root: H256,
        address: H256,
    ) -> Result<(MerkleProof, H256), TrieError> {
        let db = Arc::new(HashDatabase::from_root(self.tx, root)?);
        let hasher = Arc::new(HasherKeccak::new());

        let trie = PatriciaTrie::from(Arc::clone(&db), Arc::clone(&hasher), root.as_bytes())?;
        let proof = trie.get_proof(address.as_bytes())?;

        let Some(account) = trie.get(address.as_slice())? else { return Ok((proof, KECCAK_EMPTY)) };

        let storage_root = EthAccount::decode(&mut account.as_slice())?.storage_root;

        Ok((proof, storage_root))
    }

    /// Returns a Merkle proof of the given storage keys, starting at the given root hash.
    pub fn generate_storage_proofs(
        &self,
        storage_root: H256,
        address: H256,
        keys: &[H256],
    ) -> Result<Vec<MerkleProof>, TrieError> {
        let db = Arc::new(DupHashDatabase::from_root(self.tx, address, storage_root)?);
        let hasher = Arc::new(HasherKeccak::new());

        let trie =
            PatriciaTrie::from(Arc::clone(&db), Arc::clone(&hasher), storage_root.as_bytes())?;

        let proof =
            keys.iter().map(|key| trie.get_proof(key.as_bytes())).collect::<Result<Vec<_>, _>>()?;

        Ok(proof)
    }
}

#[cfg(test)]
mod tests {
    use crate::Transaction;
    use std::ops::DerefMut;

    use super::*;
    use assert_matches::assert_matches;
    use proptest::{prelude::ProptestConfig, proptest};
    use reth_db::{
        database::{Database, DatabaseGAT},
        mdbx::{test_utils::create_test_rw_db, Env, WriteMap},
        tables,
        transaction::DbTxMut,
    };
    use reth_primitives::{
        hex_literal::hex,
        keccak256,
        proofs::{genesis_state_root, KeccakHasher, EMPTY_ROOT},
        Address, Bytes, ChainSpec, Genesis, MAINNET,
    };
    use std::{collections::HashMap, ops::Deref, str::FromStr};
    use triehash::sec_trie_root;

    fn load_mainnet_genesis_root<DB: Database>(tx: &mut Transaction<'_, DB>) -> Genesis {
        let ChainSpec { genesis, .. } = MAINNET.clone();

        // Insert account state
        for (address, account) in &genesis.alloc {
            tx.put::<tables::PlainAccountState>(
                *address,
                Account {
                    nonce: account.nonce.unwrap_or_default(),
                    balance: account.balance,
                    bytecode_hash: None,
                },
            )
            .unwrap();
            tx.put::<tables::HashedAccount>(
                keccak256(address),
                Account {
                    nonce: account.nonce.unwrap_or_default(),
                    balance: account.balance,
                    bytecode_hash: None,
                },
            )
            .unwrap();
        }
        tx.commit().unwrap();

        genesis
    }

    fn create_test_loader<'tx, 'db>(
        tx: &'tx Transaction<'db, Env<WriteMap>>,
    ) -> DBTrieLoader<'tx, <Arc<Env<WriteMap>> as DatabaseGAT<'db>>::TXMut> {
        DBTrieLoader::new(tx.deref())
    }

    #[test]
    fn empty_trie() {
        let db = create_test_rw_db();
        let tx = Transaction::new(db.as_ref()).unwrap();
        assert_matches!(
            create_test_loader(&tx).calculate_root(),
            Ok(got) if got.root().unwrap() == EMPTY_ROOT
        );
    }

    #[test]
    fn single_account_trie() {
        let db = create_test_rw_db();
        let tx = Transaction::new(db.as_ref()).unwrap();
        let address = Address::from_str("9fe4abd71ad081f091bd06dd1c16f7e92927561e").unwrap();
        let account = Account { nonce: 0, balance: U256::ZERO, bytecode_hash: None };
        tx.put::<tables::HashedAccount>(keccak256(address), account).unwrap();
        let mut encoded_account = Vec::new();
        EthAccount::from(account).encode(&mut encoded_account);
        let expected = H256(sec_trie_root::<KeccakHasher, _, _, _>([(address, encoded_account)]).0);
        assert_matches!(
            create_test_loader(&tx).calculate_root(),
            Ok(got) if got.root().unwrap() == expected
        );
    }

    #[test]
    fn two_accounts_trie() {
        let db = create_test_rw_db();
        let mut tx = Transaction::new(db.as_ref()).unwrap();
        let mut trie = DBTrieLoader::new(tx.deref_mut());

        let accounts = [
            (
                Address::from(hex!("9fe4abd71ad081f091bd06dd1c16f7e92927561e")),
                Account { nonce: 155, balance: U256::from(414241124), bytecode_hash: None },
            ),
            (
                Address::from(hex!("f8a6edaad4a332e6e550d0915a7fd5300b0b12d1")),
                Account { nonce: 3, balance: U256::from(78978), bytecode_hash: None },
            ),
        ];
        for (address, account) in accounts {
            trie.tx.put::<tables::HashedAccount>(keccak256(address), account).unwrap();
        }
        let encoded_accounts = accounts.iter().map(|(k, v)| {
            let mut out = Vec::new();
            EthAccount::from(*v).encode(&mut out);
            (k, out)
        });
        let expected = H256(sec_trie_root::<KeccakHasher, _, _, _>(encoded_accounts).0);
        assert_matches!(
            trie.calculate_root(),
            Ok(got) if got.root().unwrap() == expected
        );
    }

    #[test]
    fn single_storage_trie() {
        let db = create_test_rw_db();
        let mut tx = Transaction::new(db.as_ref()).unwrap();
        let mut trie = DBTrieLoader::new(tx.deref_mut());

        let address = Address::from_str("9fe4abd71ad081f091bd06dd1c16f7e92927561e").unwrap();
        let hashed_address = keccak256(address);

        let storage = Vec::from([(H256::from_low_u64_be(2), U256::from(1))]);
        for (k, v) in storage.clone() {
            trie.tx
                .put::<tables::HashedStorage>(
                    hashed_address,
                    StorageEntry { key: keccak256(k), value: v },
                )
                .unwrap();
        }
        let encoded_storage = storage.iter().map(|(k, v)| {
            let out = encode_fixed_size(v).to_vec();
            (k, out)
        });
        let expected = H256(sec_trie_root::<KeccakHasher, _, _, _>(encoded_storage).0);
        let storage_trie_cursor =
            Arc::new(Mutex::new(trie.tx.cursor_dup_write::<tables::StoragesTrie>().unwrap()));
        let mut storage_cursor = trie.tx.cursor_dup_read::<tables::HashedStorage>().unwrap();

        assert_matches!(
            trie.calculate_storage_root(hashed_address,&mut storage_cursor, storage_trie_cursor, None, None),
            Ok(got) if got.root().unwrap() == expected
        );
    }

    #[test]
    fn single_account_with_storage_trie() {
        let db = create_test_rw_db();
        let tx = Transaction::new(db.as_ref()).unwrap();

        let address = Address::from_str("9fe4abd71ad081f091bd06dd1c16f7e92927561e").unwrap();
        let hashed_address = keccak256(address);

        let storage = HashMap::from([
            (H256::zero(), U256::from(3)),
            (H256::from_low_u64_be(2), U256::from(1)),
        ]);
        let code = "el buen fla";
        let account = Account {
            nonce: 155,
            balance: U256::from(414241124u32),
            bytecode_hash: Some(keccak256(code)),
        };
        tx.put::<tables::HashedAccount>(hashed_address, account).unwrap();

        for (k, v) in storage.clone() {
            tx.put::<tables::HashedStorage>(
                hashed_address,
                StorageEntry { key: keccak256(k), value: v },
            )
            .unwrap();
        }
        let mut out = Vec::new();

        let encoded_storage = storage.iter().map(|(k, v)| {
            let out = encode_fixed_size(v).to_vec();
            (k, out)
        });

        let storage_root = H256(sec_trie_root::<KeccakHasher, _, _, _>(encoded_storage).0);
        let eth_account = EthAccount::from(account).with_storage_root(storage_root);
        eth_account.encode(&mut out);

        let expected = H256(sec_trie_root::<KeccakHasher, _, _, _>([(address, out)]).0);
        assert_matches!(
            create_test_loader(&tx).calculate_root(),
            Ok(got) if got.root().unwrap() == expected
        );
    }

    #[test]
    fn verify_genesis() {
        let db = create_test_rw_db();
        let mut tx = Transaction::new(db.as_ref()).unwrap();

        let genesis = load_mainnet_genesis_root(&mut tx);

        let state_root = genesis_state_root(&genesis.alloc);

        assert_matches!(
            create_test_loader(&tx).calculate_root(),
            Ok(got) if got.root().unwrap() == state_root
        );
    }

    #[test]
    fn gather_changes() {
        let db = create_test_rw_db();
        let tx = Transaction::new(db.as_ref()).unwrap();

        let address = Address::from_str("9fe4abd71ad081f091bd06dd1c16f7e92927561e").unwrap();
        let hashed_address = keccak256(address);

        let storage = HashMap::from([
            (H256::zero(), U256::from(3)),
            (H256::from_low_u64_be(2), U256::from(1)),
        ]);
        let code = "el buen fla";
        let account = Account {
            nonce: 155,
            balance: U256::from(414241124u32),
            bytecode_hash: Some(keccak256(code)),
        };
        tx.put::<tables::HashedAccount>(hashed_address, account).unwrap();
        tx.put::<tables::AccountChangeSet>(31, AccountBeforeTx { address, info: None }).unwrap();

        for (k, v) in storage {
            tx.put::<tables::HashedStorage>(
                hashed_address,
                StorageEntry { key: keccak256(k), value: v },
            )
            .unwrap();
            tx.put::<tables::StorageChangeSet>(
                (32, address).into(),
                StorageEntry { key: k, value: U256::ZERO },
            )
            .unwrap();
        }

        let expected = BTreeMap::from([(
            hashed_address,
            BTreeSet::from([keccak256(H256::zero()), keccak256(H256::from_low_u64_be(2))]),
        )]);
        assert_matches!(
            create_test_loader(&tx).gather_changes(32..33),
            Ok(got) if got == expected
        );
    }

    #[test]
    fn update_storage_root() {
        let db = create_test_rw_db();
        let mut tx = Transaction::new(db.as_ref()).unwrap();

        // Addresses
        let address = Address::from_str("9fe4abd71ad081f091bd06dd1c16f7e92927561e").unwrap();
        let hashed_address = keccak256(address);

        // Storage
        let storage = HashMap::from([
            (H256::zero(), U256::from(3)),
            (H256::from_low_u64_be(2), U256::from(1)),
        ]);
        let code = "el buen fla";
        let account = Account {
            nonce: 155,
            balance: U256::from(414241124u32),
            bytecode_hash: Some(keccak256(code)),
        };

        // Insert hashed account
        tx.put::<tables::HashedAccount>(hashed_address, account).unwrap();

        // Insert hashed storage
        for (k, v) in storage.clone() {
            tx.put::<tables::HashedStorage>(
                hashed_address,
                StorageEntry { key: keccak256(k), value: v },
            )
            .unwrap();
        }
        let mut out = Vec::new();
        let encoded_storage = storage.iter().map(|(k, v)| {
            let out = encode_fixed_size(v).to_vec();
            (k, out)
        });

        // Calculate expected roots
        let storage_root = H256(sec_trie_root::<KeccakHasher, _, _, _>(encoded_storage).0);
        let eth_account = EthAccount::from(account).with_storage_root(storage_root);
        eth_account.encode(&mut out);
        let expected = H256(sec_trie_root::<KeccakHasher, _, _, _>([(address, out)]).0);

        // Check that the root matched
        let got = create_test_loader(&tx).calculate_root().unwrap().root().unwrap();
        assert_matches!(got, expected);
        let prev_root_hash = got;
        // Commit
        tx.commit().unwrap();

        // Update storage (slot 0 changes from 3 to 4, slot 2 changes from 1 to 2)
        // New storage:
        // - Slot 0 changes from 3 to 4
        // - Slot 2 changes from 1 to 2
        // - Slot 3 is created with value 5
        let new_storage = HashMap::from([
            (H256::zero(), U256::from(4)),
            (H256::from_low_u64_be(2), U256::from(2)),
            (H256::from_low_u64_be(3), U256::from(5)),
        ]);

        // Insert hashed storage and changeset
        tx.clear::<tables::HashedStorage>().unwrap();
        for (k, v) in new_storage.clone() {
            tx.put::<tables::HashedStorage>(
                hashed_address,
                StorageEntry { key: keccak256(k), value: v },
            )
            .unwrap();
            tx.put::<tables::StorageChangeSet>(
                (32, address).into(),
                StorageEntry { key: k, value: storage.get(&k).cloned().unwrap_or_default() },
            )
            .unwrap();
        }

        let encoded_storage = new_storage.iter().map(|(k, v)| {
            let out = encode_fixed_size(v).to_vec();
            (k, out)
        });

        // Calculate expected roots
        let mut out = Vec::new();
        let storage_root = H256(sec_trie_root::<KeccakHasher, _, _, _>(encoded_storage).0);
        let eth_account = EthAccount::from(account).with_storage_root(storage_root);
        eth_account.encode(&mut out);
        let expected = H256(sec_trie_root::<KeccakHasher, _, _, _>([(address, out)]).0);

        // Check that the root matched
        {
            let mut loader = DBTrieLoader::new_with_threshold(tx.deref(), 1);
            assert_matches!(
                loader.update_root(prev_root_hash, 32..33),
                // todo: check
                Ok(TrieProgress::InProgress(_))
            );
            tx.commit().unwrap();
        }
        {
            let mut loader = DBTrieLoader::new_with_threshold(tx.deref(), 1);
            assert_matches!(
                loader.update_root(prev_root_hash, 32..33),
                // todo: check
                Ok(TrieProgress::InProgress(_))
            );
            tx.commit().unwrap();
        }
        {
            let mut loader = DBTrieLoader::new_with_threshold(tx.deref(), 1);
            assert_matches!(
                loader.update_root(prev_root_hash, 32..33),
                // todo: check
                Ok(got) if got.root().unwrap() == expected
            );
        }
    }

    fn test_with_accounts(accounts: BTreeMap<Address, (Account, BTreeSet<StorageEntry>)>) {
        let db = create_test_rw_db();
        let tx = Transaction::new(db.as_ref()).unwrap();

        let encoded_accounts = accounts
            .into_iter()
            .map(|(address, (account, storage))| {
                let hashed_address = keccak256(address);
                tx.put::<tables::HashedAccount>(hashed_address, account).unwrap();
                // This is to mimic real data. Only contract accounts have storage.
                let storage_root = if account.has_bytecode() {
                    let encoded_storage = storage.into_iter().map(|StorageEntry { key, value }| {
                        let hashed_key = keccak256(key);
                        let out = encode_fixed_size(&value).to_vec();
                        tx.put::<tables::HashedStorage>(
                            hashed_address,
                            StorageEntry { key: hashed_key, value },
                        )
                        .unwrap();
                        (key, out)
                    });
                    H256(sec_trie_root::<KeccakHasher, _, _, _>(encoded_storage).0)
                } else {
                    EMPTY_ROOT
                };
                let mut out = Vec::new();
                EthAccount::from(account).with_storage_root(storage_root).encode(&mut out);
                (address, out)
            })
            .collect::<Vec<(Address, Vec<u8>)>>();

        let expected = H256(sec_trie_root::<KeccakHasher, _, _, _>(encoded_accounts).0);
        assert_matches!(
            create_test_loader(&tx).calculate_root(),
            Ok(got) if got.root().unwrap() == expected
        , "where expected is {expected:?}");
    }

    #[test]
    fn arbitrary() {
        proptest!(ProptestConfig::with_cases(10), |(accounts: BTreeMap<Address, (Account, BTreeSet<StorageEntry>)>)| {
            test_with_accounts(accounts);
        });
    }

    #[test]
    fn get_proof() {
        let db = create_test_rw_db();
        let mut tx = Transaction::new(db.as_ref()).unwrap();

        load_mainnet_genesis_root(&mut tx);

        let root = {
            let mut trie = create_test_loader(&tx);
            trie.calculate_root().expect("should be able to load trie").root().unwrap()
        };

        tx.commit().unwrap();

        let address = Address::from(hex!("000d836201318ec6899a67540690382780743280"));

        let trie = create_test_loader(&tx);
        let (proof, storage_root) =
            trie.generate_acount_proof(root, keccak256(address)).expect("failed to generate proof");

        // values extracted from geth via rpc:
        // {
        //  "method": "eth_getProof",
        //  "params": ["0x000d836201318ec6899a67540690382780743280", [], "0x0"]
        // }
        let expected = [
            hex!("f90211a090dcaf88c40c7bbc95a912cbdde67c175767b31173df9ee4b0d733bfdd511c43a0babe369f6b12092f49181ae04ca173fb68d1a5456f18d20fa32cba73954052bda0473ecf8a7e36a829e75039a3b055e51b8332cbf03324ab4af2066bbd6fbf0021a0bbda34753d7aa6c38e603f360244e8f59611921d9e1f128372fec0d586d4f9e0a04e44caecff45c9891f74f6a2156735886eedf6f1a733628ebc802ec79d844648a0a5f3f2f7542148c973977c8a1e154c4300fec92f755f7846f1b734d3ab1d90e7a0e823850f50bf72baae9d1733a36a444ab65d0a6faaba404f0583ce0ca4dad92da0f7a00cbe7d4b30b11faea3ae61b7f1f2b315b61d9f6bd68bfe587ad0eeceb721a07117ef9fc932f1a88e908eaead8565c19b5645dc9e5b1b6e841c5edbdfd71681a069eb2de283f32c11f859d7bcf93da23990d3e662935ed4d6b39ce3673ec84472a0203d26456312bbc4da5cd293b75b840fc5045e493d6f904d180823ec22bfed8ea09287b5c21f2254af4e64fca76acc5cd87399c7f1ede818db4326c98ce2dc2208a06fc2d754e304c48ce6a517753c62b1a9c1d5925b89707486d7fc08919e0a94eca07b1c54f15e299bd58bdfef9741538c7828b5d7d11a489f9c20d052b3471df475a051f9dd3739a927c89e357580a4c97b40234aa01ed3d5e0390dc982a7975880a0a089d613f26159af43616fd9455bb461f4869bfede26f2130835ed067a8b967bfb80").as_slice(),
            hex!("f90211a0dae48f5b47930c28bb116fbd55e52cd47242c71bf55373b55eb2805ee2e4a929a00f1f37f337ec800e2e5974e2e7355f10f1a4832b39b846d916c3597a460e0676a0da8f627bb8fbeead17b318e0a8e4f528db310f591bb6ab2deda4a9f7ca902ab5a0971c662648d58295d0d0aa4b8055588da0037619951217c22052802549d94a2fa0ccc701efe4b3413fd6a61a6c9f40e955af774649a8d9fd212d046a5a39ddbb67a0d607cdb32e2bd635ee7f2f9e07bc94ddbd09b10ec0901b66628e15667aec570ba05b89203dc940e6fa70ec19ad4e01d01849d3a5baa0a8f9c0525256ed490b159fa0b84227d48df68aecc772939a59afa9e1a4ab578f7b698bdb1289e29b6044668ea0fd1c992070b94ace57e48cbf6511a16aa770c645f9f5efba87bbe59d0a042913a0e16a7ccea6748ae90de92f8aef3b3dc248a557b9ac4e296934313f24f7fced5fa042373cf4a00630d94de90d0a23b8f38ced6b0f7cb818b8925fee8f0c2a28a25aa05f89d2161c1741ff428864f7889866484cef622de5023a46e795dfdec336319fa07597a017664526c8c795ce1da27b8b72455c49657113e0455552dbc068c5ba31a0d5be9089012fda2c585a1b961e988ea5efcd3a06988e150a8682091f694b37c5a0f7b0352e38c315b2d9a14d51baea4ddee1770974c806e209355233c3c89dce6ea049bf6e8df0acafd0eff86defeeb305568e44d52d2235cf340ae15c6034e2b24180").as_slice(),
            hex!("f901f1a0cf67e0f5d5f8d70e53a6278056a14ddca46846f5ef69c7bde6810d058d4a9eda80a06732ada65afd192197fe7ce57792a7f25d26978e64e954b7b84a1f7857ac279da05439f8d011683a6fc07efb90afca198fd7270c795c835c7c85d91402cda992eaa0449b93033b6152d289045fdb0bf3f44926f831566faa0e616b7be1abaad2cb2da031be6c3752bcd7afb99b1bb102baf200f8567c394d464315323a363697646616a0a40e3ed11d906749aa501279392ffde868bd35102db41364d9c601fd651f974aa0044bfa4fe8dd1a58e6c7144da79326e94d1331c0b00373f6ae7f3662f45534b7a098005e3e48db68cb1dc9b9f034ff74d2392028ddf718b0f2084133017da2c2e7a02a62bc40414ee95b02e202a9e89babbabd24bef0abc3fc6dcd3e9144ceb0b725a0239facd895bbf092830390a8676f34b35b29792ae561f196f86614e0448a5792a0a4080f88925daff6b4ce26d188428841bd65655d8e93509f2106020e76d41eefa04918987904be42a6894256ca60203283d1b89139cf21f09f5719c44b8cdbb8f7a06201fc3ef0827e594d953b5e3165520af4fceb719e11cc95fd8d3481519bfd8ca05d0e353d596bd725b09de49c01ede0f29023f0153d7b6d401556aeb525b2959ba0cd367d0679950e9c5f2aa4298fd4b081ade2ea429d71ff390c50f8520e16e30880").as_slice(),
            hex!("f87180808080808080a0dbee8b33c73b86df839f309f7ac92eee19836e08b39302ffa33921b3c6a09f66a06068b283d51aeeee682b8fb5458354315d0b91737441ede5e137c18b4775174a8080808080a0fe7779c7d58c2fda43eba0a6644043c86ebb9ceb4836f89e30831f23eb059ece8080").as_slice(),
            hex!("f8719f20b71c90b0d523dd5004cf206f325748da347685071b34812e21801f5270c4b84ff84d80890ad78ebc5ac6200000a056e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421a0c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470").as_slice(),
        ];

        assert_eq!(storage_root, EMPTY_ROOT);

        assert_eq!(proof.len(), 5);

        for (node, expected) in proof.into_iter().zip(expected.into_iter()) {
            assert_eq!(Bytes::from(node.as_slice()), Bytes::from(expected));
        }
    }

    #[test]
    fn get_storage_proofs() {
        let db = create_test_rw_db();
        let mut tx = Transaction::new(db.as_ref()).unwrap();

        let address = Address::from_str("9fe4abd71ad081f091bd06dd1c16f7e92927561e").unwrap();
        let hashed_address = keccak256(address);

        let storage = HashMap::from([
            (H256::zero(), U256::from(3)),
            (H256::from_low_u64_be(2), U256::from(1)),
        ]);

        let code = "el buen fla";
        let account = Account {
            nonce: 155,
            balance: U256::from(414241124u32),
            bytecode_hash: Some(keccak256(code)),
        };
        tx.put::<tables::HashedAccount>(hashed_address, account).unwrap();

        for (k, v) in storage {
            tx.put::<tables::HashedStorage>(
                hashed_address,
                StorageEntry { key: keccak256(k), value: v },
            )
            .unwrap();
        }

        let root = {
            let mut trie = create_test_loader(&tx);
            trie.calculate_root().expect("should be able to load trie").root().unwrap()
        };

        tx.commit().unwrap();

        let trie = create_test_loader(&tx);
        let (account_proof, storage_root) =
            trie.generate_acount_proof(root, hashed_address).expect("failed to generate proof");

        // values extracted from geth via rpc:
        let expected_account = hex!("f86fa1205126413e7857595763591580306b3f228f999498c4c5dfa74f633364936e7651b84bf849819b8418b0d164a029ff6f4d518044318d75b118cf439d8d3d7249c8afcba06ba9ecdf8959410571a02ce1a85814ad94a94ed2a1abaf7c57e9b64326622c1b8c21b4ba4d0e7df61392").as_slice();
        let expected_storage = [
            [
                // 0x0000000000000000000000000000000000000000000000000000000000000002
                hex!("f8518080a04355bd3061ad2d17e0782413925b4fd81a56bd162d91eedb2a00d6c87611471480a015503e91f9250654cf72906e38a7cb14c3f1cc06658379d37f0c5b5c32482880808080808080808080808080").as_slice(),
                hex!("e2a0305787fa12a823e0f2b7631cc41b3ba8828b3321ca811111fa75cd3aa3bb5ace01").as_slice(),
            ],
            [
                // 0x0000000000000000000000000000000000000000000000000000000000000000
                hex!("f8518080a04355bd3061ad2d17e0782413925b4fd81a56bd162d91eedb2a00d6c87611471480a015503e91f9250654cf72906e38a7cb14c3f1cc06658379d37f0c5b5c32482880808080808080808080808080").as_slice(),
                hex!("e2a0390decd9548b62a8d60345a988386fc84ba6bc95484008f6362f93160ef3e56303").as_slice(),
            ]
        ];

        assert_ne!(storage_root, EMPTY_ROOT);

        assert_eq!(account_proof.len(), 1);
        assert_eq!(account_proof[0], expected_account);

        let storage_proofs = trie
            .generate_storage_proofs(
                storage_root,
                hashed_address,
                &[keccak256(H256::from_low_u64_be(2)), keccak256(H256::zero())],
            )
            .expect("couldn't generate storage proof");

        for (proof, expected) in storage_proofs.into_iter().zip(expected_storage) {
            assert_eq!(proof.len(), expected.len());
            for (got_node, expected_node) in proof.into_iter().zip(expected) {
                assert_eq!(got_node, expected_node);
            }
        }
    }
}
