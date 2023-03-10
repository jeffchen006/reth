//! Implementation of [`BlockchainTree`]
pub mod block_indices;
pub mod chain;

pub use chain::{Chain, ChainId, ForkBlock};

use reth_db::{cursor::DbCursorRO, database::Database, tables, transaction::DbTx};
use reth_interfaces::{consensus::Consensus, executor::Error as ExecError, Error};
use reth_primitives::{BlockHash, BlockNumber, ChainSpec, SealedBlock, SealedBlockWithSenders};
use reth_provider::{
    ExecutorFactory, HeaderProvider, ShareableDatabase, StateProvider, StateProviderFactory,
    Transaction,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use self::block_indices::BlockIndices;

#[cfg_attr(doc, aquamarine::aquamarine)]
/// Tree of chains and its identifications.
///
/// Mermaid flowchart represent all blocks that can appear in blockchain.
/// Green blocks belong to canonical chain and are saved inside database table, they are our main
/// chain. Pending blocks and sidechains are found in memory inside [`BlockchainTree`].
/// Both pending and sidechains have same mechanisms only difference is when they got committed to
/// database. For pending it is just append operation but for sidechains they need to move current
/// canonical blocks to BlockchainTree flush sidechain to the database to become canonical chain.
/// ```mermaid
/// flowchart BT
/// subgraph canonical chain
/// CanonState:::state
/// block0canon:::canon -->block1canon:::canon -->block2canon:::canon -->block3canon:::canon --> block4canon:::canon --> block5canon:::canon
/// end
/// block5canon --> block6pending:::pending
/// block5canon --> block67pending:::pending
/// subgraph sidechain2
/// S2State:::state
/// block3canon --> block4s2:::sidechain --> block5s2:::sidechain
/// end
/// subgraph sidechain1
/// S1State:::state
/// block2canon --> block3s1:::sidechain --> block4s1:::sidechain --> block5s1:::sidechain --> block6s1:::sidechain
/// end
/// classDef state fill:#1882C4
/// classDef canon fill:#8AC926
/// classDef pending fill:#FFCA3A
/// classDef sidechain fill:#FF595E
/// ```
///
///
/// main functions:
/// * insert_block: insert block inside tree. Execute it and save it to database.
/// * finalize_block: Flush chain that joins to finalized block.
/// * make_canonical: Check if we have the hash of block that we want to finalize and commit it to
///   db. If we dont have the block pipeline syncing should start to fetch the blocks from p2p.
/// *
/// Do reorg if needed

pub struct BlockchainTree<DB: Database, C: Consensus, EF: ExecutorFactory> {
    /// chains and present data
    pub chains: HashMap<ChainId, Chain>,
    /// Static chain id generator
    pub chain_id_generator: u64,
    /// Indices to block and their connection.
    pub block_indices: BlockIndices,
    /// Number of block after finalized block that we are storing. It should be more then
    /// finalization window
    pub num_of_side_chain_max_size: u64,
    /// Finalization windows. Number of blocks that can be reorged
    pub finalization_window: u64,
    /// Externals
    pub externals: Externals<DB, C, EF>,
}

/// Container for external abstractions.
pub struct Externals<DB: Database, C: Consensus, EF: ExecutorFactory> {
    /// Save sidechain, do reorgs and push new block to canonical chain that is inside db.
    pub db: DB,
    /// Consensus checks
    pub consensus: C,
    /// Create executor to execute blocks.
    pub executor_factory: EF,
    /// Chain spec
    pub chain_spec: Arc<ChainSpec>,
}

impl<DB: Database, C: Consensus, EF: ExecutorFactory> Externals<DB, C, EF> {
    /// Return sharable database helper structure.
    pub fn sharable_db(&self) -> ShareableDatabase<&DB> {
        ShareableDatabase::new(&self.db, self.chain_spec.clone())
    }
}

/// Helper structure that wraps chains and indices to search for block hash accross the chains.
pub struct BlockHashes<'a> {
    /// Chains
    pub chains: &'a mut HashMap<ChainId, Chain>,
    /// Indices
    pub indices: &'a BlockIndices,
}

impl<DB: Database, C: Consensus, EF: ExecutorFactory> BlockchainTree<DB, C, EF> {
    /// New blockchain tree
    pub fn new(
        externals: Externals<DB, C, EF>,
        finalization_window: u64,
        num_of_side_chain_max_size: u64,
        num_of_additional_canonical_block_hashes: u64,
    ) -> Result<Self, Error> {
        if finalization_window > num_of_side_chain_max_size {
            panic!("Side chain size should be more then finalization window");
        }

        let last_canonical_hashes = externals
            .db
            .tx()?
            .cursor_read::<tables::CanonicalHeaders>()?
            .walk_back(None)?
            .take((finalization_window + num_of_additional_canonical_block_hashes) as usize)
            .collect::<Result<Vec<(BlockNumber, BlockHash)>, _>>()?;

        // TODO(rakita) save last finalized block inside database but for now just take
        // tip-finalization_window
        let (last_finalized_block_number, _) =
            if last_canonical_hashes.len() > finalization_window as usize {
                last_canonical_hashes[finalization_window as usize]
            } else {
                // it is in reverse order from tip to N
                last_canonical_hashes.last().cloned().unwrap_or_default()
            };

        Ok(Self {
            externals,
            chain_id_generator: 0,
            chains: Default::default(),
            block_indices: BlockIndices::new(
                last_finalized_block_number,
                num_of_additional_canonical_block_hashes,
                BTreeMap::from_iter(last_canonical_hashes.into_iter()),
            ),
            num_of_side_chain_max_size,
            finalization_window,
        })
    }

    /// Fork side chain or append the block if parent is the top of the chain
    fn fork_side_chain(
        &mut self,
        block: SealedBlockWithSenders,
        chain_id: ChainId,
    ) -> Result<(), Error> {
        let block_hashes = self.all_chain_hashes(chain_id);

        // get canonical fork.
        let canonical_fork =
            self.canonical_fork(chain_id).ok_or(ExecError::ChainIdConsistency { chain_id })?;

        // get chain that block needs to join to.
        let parent_chain =
            self.chains.get_mut(&chain_id).ok_or(ExecError::ChainIdConsistency { chain_id })?;
        let chain_tip = parent_chain.tip().hash();

        let canonical_block_hashes = self.block_indices.canonical_chain();

        // get canonical tip
        let (_, canonical_tip_hash) =
            canonical_block_hashes.last_key_value().map(|(i, j)| (*i, *j)).unwrap_or_default();

        let db = self.externals.sharable_db();
        let provider = if canonical_fork.hash == canonical_tip_hash {
            Box::new(db.latest()?) as Box<dyn StateProvider>
        } else {
            Box::new(db.history_by_block_number(canonical_fork.number)?) as Box<dyn StateProvider>
        };

        // append the block if it is continuing the chain.
        if chain_tip == block.parent_hash {
            parent_chain.append_block(
                block,
                block_hashes,
                canonical_block_hashes,
                &provider,
                &self.externals.consensus,
                &self.externals.executor_factory,
            )?;
        } else {
            let chain = parent_chain.new_chain_fork(
                block,
                block_hashes,
                canonical_block_hashes,
                &provider,
                &self.externals.consensus,
                &self.externals.executor_factory,
            )?;
            // release the lifetime with a drop
            drop(provider);
            self.insert_chain(chain);
        }

        Ok(())
    }

    /// Fork canonical chain by creating new chain
    pub fn fork_canonical_chain(&mut self, block: SealedBlockWithSenders) -> Result<(), Error> {
        let canonical_block_hashes = self.block_indices.canonical_chain();
        let (_, canonical_tip) =
            canonical_block_hashes.last_key_value().map(|(i, j)| (*i, *j)).unwrap_or_default();

        // create state provider
        let db = self.externals.sharable_db();
        let parent_header = db
            .header(&block.parent_hash)?
            .ok_or(ExecError::CanonicalChain { block_hash: block.parent_hash })?;

        let provider = if block.parent_hash == canonical_tip {
            Box::new(db.latest()?) as Box<dyn StateProvider>
        } else {
            Box::new(db.history_by_block_number(block.number - 1)?) as Box<dyn StateProvider>
        };

        let parent_header = parent_header.seal(block.parent_hash);
        let chain = Chain::new_canonical_fork(
            &block,
            parent_header,
            canonical_block_hashes,
            &provider,
            &self.externals.consensus,
            &self.externals.executor_factory,
        )?;
        drop(provider);
        self.insert_chain(chain);
        Ok(())
    }

    /// Get all block hashes from chain that are not canonical. This is one time operation per
    /// block. Reason why this is not caches is to save memory.
    fn all_chain_hashes(&self, chain_id: ChainId) -> BTreeMap<BlockNumber, BlockHash> {
        // find chain and iterate over it,
        let mut chain_id = chain_id;
        let mut hashes = BTreeMap::new();
        loop {
            let Some(chain) = self.chains.get(&chain_id) else { return hashes};
            hashes.extend(chain.blocks.values().map(|b| (b.number, b.hash())));

            let fork_block = chain.fork_block_hash();
            if let Some(next_chain_id) = self.block_indices.get_blocks_chain_id(&fork_block) {
                chain_id = next_chain_id;
            } else {
                // if there is no fork block that point to other chains, break the loop.
                // it means that this fork joins to canonical block.
                break
            }
        }
        hashes
    }

    /// Getting the canonical fork would tell use what kind of Provider we should execute block on.
    /// If it is latest state provider or history state provider
    /// Return None if chain_id is not known.
    fn canonical_fork(&self, chain_id: ChainId) -> Option<ForkBlock> {
        let mut chain_id = chain_id;
        let mut fork;
        loop {
            // chain fork block
            fork = self.chains.get(&chain_id)?.fork_block();
            // get fork block chain
            if let Some(fork_chain_id) = self.block_indices.get_blocks_chain_id(&fork.hash) {
                chain_id = fork_chain_id;
                continue
            }
            break
        }
        if self.block_indices.canonical_hash(&fork.number) == Some(fork.hash) {
            Some(fork)
        } else {
            None
        }
    }

    /// Insert chain to tree and ties the blocks to it.
    /// Helper function that handles indexing and inserting.
    fn insert_chain(&mut self, chain: Chain) -> ChainId {
        let chain_id = self.chain_id_generator;
        self.chain_id_generator += 1;
        self.block_indices.insert_chain(chain_id, &chain);
        // add chain_id -> chain index
        self.chains.insert(chain_id, chain);
        chain_id
    }

    /// Insert block inside tree. recover transaction signers and
    /// internaly call [`BlockchainTree::insert_block_with_senders`] fn.
    pub fn insert_block(&mut self, block: SealedBlock) -> Result<bool, Error> {
        let senders = block.senders().ok_or(ExecError::SenderRecoveryError)?;
        let block = SealedBlockWithSenders::new(block, senders).unwrap();
        self.insert_block_with_senders(&block)
    }

    /// Insert block with senders inside tree
    pub fn insert_block_with_senders(
        &mut self,
        block: &SealedBlockWithSenders,
    ) -> Result<bool, Error> {
        // check if block number is inside pending block slide
        let last_finalized_block = self.block_indices.last_finalized_block();
        if block.number <= last_finalized_block {
            return Err(ExecError::PendingBlockIsFinalized {
                block_number: block.number,
                block_hash: block.hash(),
                last_finalized: last_finalized_block,
            }
            .into())
        }

        // we will not even try to insert blocks that are too far in future.
        if block.number > last_finalized_block + self.num_of_side_chain_max_size {
            return Err(ExecError::PendingBlockIsInFuture {
                block_number: block.number,
                block_hash: block.hash(),
                last_finalized: last_finalized_block,
            }
            .into())
        }

        // check if block is already inside Tree
        if self.block_indices.contains_block_hash(block.hash()) {
            // block is known return that is inserted
            return Ok(true)
        }

        // check if block is part of canonical chain
        if self.block_indices.canonical_hash(&block.number) == Some(block.hash()) {
            // block is part of canonical chain
            return Ok(true)
        }

        // check if block parent can be found in Tree
        if let Some(parent_chain) = self.block_indices.get_blocks_chain_id(&block.parent_hash) {
            self.fork_side_chain(block.clone(), parent_chain)?;
            //self.db.tx_mut()?.put::<tables::PendingBlocks>(block.hash(), block.unseal())?;
            return Ok(true)
        }

        // if not found, check if it can be found inside canonical chain.
        if Some(block.parent_hash) == self.block_indices.canonical_hash(&(block.number - 1)) {
            // create new chain that points to that block
            self.fork_canonical_chain(block.clone())?;
            //self.db.tx_mut()?.put::<tables::PendingBlocks>(block.hash(), block.unseal())?;
            return Ok(true)
        }
        // NOTE: Block doesn't have a parent, and if we receive this block in `make_canonical`
        // function this could be a trigger to initiate p2p syncing, as we are missing the
        // parent.
        Ok(false)
    }

    /// Do finalization of blocks. Remove them from tree
    pub fn finalize_block(&mut self, finalized_block: BlockNumber) {
        let mut remove_chains = self.block_indices.finalize_canonical_blocks(finalized_block);

        while let Some(chain_id) = remove_chains.pop_first() {
            if let Some(chain) = self.chains.remove(&chain_id) {
                remove_chains.extend(self.block_indices.remove_chain(&chain));
            }
        }
    }

    /// Update canonical hashes. Reads last N canonical blocks from database and update all indices.
    pub fn update_canonical_hashes(
        &mut self,
        last_finalized_block: BlockNumber,
    ) -> Result<(), Error> {
        self.finalize_block(last_finalized_block);

        let num_of_canonical_hashes =
            self.finalization_window + self.block_indices.num_of_additional_canonical_block_hashes;

        let last_canonical_hashes = self
            .externals
            .db
            .tx()?
            .cursor_read::<tables::CanonicalHeaders>()?
            .walk_back(None)?
            .take(num_of_canonical_hashes as usize)
            .collect::<Result<BTreeMap<BlockNumber, BlockHash>, _>>()?;

        let mut remove_chains = self.block_indices.update_block_hashes(last_canonical_hashes);

        // remove all chains that got discarded
        while let Some(chain_id) = remove_chains.first() {
            if let Some(chain) = self.chains.remove(chain_id) {
                remove_chains.extend(self.block_indices.remove_chain(&chain));
            }
        }

        Ok(())
    }

    /// Make block and its parent canonical. Unwind chains to database if necessary.
    ///
    /// If block is alreadt
    pub fn make_canonical(&mut self, block_hash: &BlockHash) -> Result<(), Error> {
        let chain_id = if let Some(chain_id) = self.block_indices.get_blocks_chain_id(block_hash) {
            chain_id
        } else {
            if self.block_indices.is_block_hash_canonical(block_hash) {
                // If block is already canonical don't return error.
                return Ok(())
            }
            return Err(ExecError::BlockHashNotFoundInChain { block_hash: *block_hash }.into())
        };
        let chain = self.chains.remove(&chain_id).expect("To be present");

        // we are spliting chain as there is possibility that only part of chain get canonicalized.
        let (canonical, pending) = chain.split_at_block_hash(block_hash);
        let canonical = canonical.expect("Canonical chain is present");

        if let Some(pending) = pending {
            // fork is now canonical and latest.
            self.block_indices.insert_chain(chain_id, &pending);
            self.chains.insert(chain_id, pending);
        }

        let mut block_fork = canonical.fork_block();
        let mut block_fork_number = canonical.fork_block_number();
        let mut chains_to_promote = vec![canonical];

        // loop while fork blocks are found in Tree.
        while let Some(chain_id) = self.block_indices.get_blocks_chain_id(&block_fork.hash) {
            let chain = self.chains.remove(&chain_id).expect("To fork to be present");
            block_fork = chain.fork_block();
            let (canonical, rest) = chain.split_at_number(block_fork_number);
            let canonical = canonical.expect("Chain is present");
            // reinsert back the chunk of sidechain that didn't get reorged.
            if let Some(rest_of_sidechain) = rest {
                self.block_indices.insert_chain(chain_id, &rest_of_sidechain);
                self.chains.insert(chain_id, rest_of_sidechain);
            }
            block_fork_number = canonical.fork_block_number();
            chains_to_promote.push(canonical);
        }

        let old_tip = self.block_indices.canonical_tip();
        // Merge all chain into one chain.
        let mut new_canon_chain = chains_to_promote.pop().expect("There is at least one block");
        for chain in chains_to_promote.into_iter().rev() {
            new_canon_chain.append_chain(chain);
        }

        // update canonical index
        self.block_indices.canonicalize_blocks(&new_canon_chain.blocks);

        // if joins to the tip
        if new_canon_chain.fork_block_hash() == old_tip.hash {
            // append to database
            self.commit_canonical(new_canon_chain)?;
        } else {
            // it forks to canonical block that is not the tip.

            let canon_fork = new_canon_chain.fork_block();
            // sanity check
            if self.block_indices.canonical_hash(&canon_fork.number) != Some(canon_fork.hash) {
                unreachable!("all chains should point to canonical chain.");
            }

            // revert `N` blocks from current canonical chain and put them inside BlockchanTree
            // This is main reorgs on tables.
            let old_canon_chain = self.revert_canonical(canon_fork.number)?;
            self.commit_canonical(new_canon_chain)?;

            // insert old canonical chain to BlockchainTree.
            // TODO check if there there is chains that can be merged.
            self.insert_chain(old_canon_chain);
        }

        Ok(())
    }

    /// Commit chain for it to become canonical. Assume we are doing pending operation to db.
    fn commit_canonical(&mut self, chain: Chain) -> Result<(), Error> {
        let mut tx = Transaction::new(&self.externals.db)?;

        let new_tip = chain.tip().number;

        for item in chain.blocks.into_iter().zip(chain.changesets.into_iter()) {
            let ((_, block), changeset) = item;
            tx.insert_block(block, self.externals.chain_spec.as_ref(), changeset).map_err(|e| {
                println!("commit error:{e:?}");
                ExecError::VerificationFailed
            })?;
        }
        // update pipeline progress.
        tx.update_pipeline_stages(new_tip).map_err(|_| ExecError::VerificationFailed)?;

        // TODO error cast

        tx.commit()?;

        Ok(())
    }

    /// Revert canonical blocks from database and insert them to pending table
    /// Revert should be non inclusive, and revert_until should stay in db.
    /// Return the chain that represent reverted canonical blocks.
    fn revert_canonical(&mut self, revert_until: BlockNumber) -> Result<Chain, Error> {
        // read data that is needed for new sidechain

        let mut tx = Transaction::new(&self.externals.db)?;

        // read block and execution result from database. and remove traces of block from tables.
        let blocks_and_execution = tx
            .get_block_and_execution_range::<true>(
                self.externals.chain_spec.as_ref(),
                (revert_until + 1)..,
            )
            .map_err(|e| {
                println!("revert error:{e:?}");
                ExecError::VerificationFailed
            })?;

        // update pipeline progress.
        tx.update_pipeline_stages(revert_until).map_err(|_| ExecError::VerificationFailed)?;
        // TODO error cast

        tx.commit()?;

        let chain = Chain::new(blocks_and_execution);

        Ok(chain)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use parking_lot::Mutex;
    use reth_db::{
        mdbx::{test_utils::create_test_rw_db, Env, WriteMap},
        transaction::DbTxMut,
    };
    use reth_interfaces::test_utils::TestConsensus;
    use reth_primitives::{hex_literal::hex, proofs::EMPTY_ROOT, ChainSpecBuilder, H256, MAINNET};
    use reth_provider::{
        execution_result::ExecutionResult, insert_block, test_utils::blocks, BlockExecutor,
    };

    struct TestFactory {
        exec_result: Arc<Mutex<Vec<ExecutionResult>>>,
        chain_spec: Arc<ChainSpec>,
    }

    impl TestFactory {
        fn new(chain_spec: Arc<ChainSpec>) -> Self {
            Self { exec_result: Arc::new(Mutex::new(Vec::new())), chain_spec }
        }

        fn extend(&self, exec_res: Vec<ExecutionResult>) {
            self.exec_result.lock().extend(exec_res.into_iter());
        }
    }

    struct TestExecutor(Option<ExecutionResult>);

    impl<SP: StateProvider> BlockExecutor<SP> for TestExecutor {
        fn execute(
            &mut self,
            _block: &reth_primitives::Block,
            _total_difficulty: reth_primitives::U256,
            _senders: Option<Vec<reth_primitives::Address>>,
        ) -> Result<ExecutionResult, ExecError> {
            self.0.clone().ok_or(ExecError::VerificationFailed)
        }

        fn execute_and_verify_receipt(
            &mut self,
            _block: &reth_primitives::Block,
            _total_difficulty: reth_primitives::U256,
            _senders: Option<Vec<reth_primitives::Address>>,
        ) -> Result<ExecutionResult, ExecError> {
            self.0.clone().ok_or(ExecError::VerificationFailed)
        }
    }

    impl ExecutorFactory for TestFactory {
        type Executor<T: StateProvider> = TestExecutor;

        fn with_sp<SP: StateProvider>(&self, _sp: SP) -> Self::Executor<SP> {
            let exec_res = self.exec_result.lock().pop();
            TestExecutor(exec_res)
        }

        fn chain_spec(&self) -> &ChainSpec {
            self.chain_spec.as_ref()
        }
    }

    type TestExternals = Externals<Arc<Env<WriteMap>>, TestConsensus, TestFactory>;

    fn externals(exec_res: Vec<ExecutionResult>) -> TestExternals {
        let db = create_test_rw_db();
        let consensus = TestConsensus::default();
        let chain_spec = Arc::new(
            ChainSpecBuilder::default()
                .chain(MAINNET.chain)
                .genesis(MAINNET.genesis.clone())
                .shanghai_activated()
                .build(),
        );
        let executor_factory = TestFactory::new(chain_spec.clone());
        executor_factory.extend(exec_res);

        Externals { db, consensus, executor_factory, chain_spec }
    }

    fn setup(externals: &TestExternals) {
        // insert genesis to db.

        let mut genesis = blocks::genesis();
        genesis.header.header.number = 10;
        genesis.header.header.state_root = EMPTY_ROOT;
        let tx_mut = externals.db.tx_mut().unwrap();

        tx_mut.put::<tables::AccountsTrie>(EMPTY_ROOT, vec![0x80]).unwrap();
        insert_block(&tx_mut, genesis.clone(), None, false, Some((0, 0))).unwrap();

        // insert first 10 blocks
        for i in 0..10 {
            tx_mut.put::<tables::CanonicalHeaders>(i, H256([100 + i as u8; 32])).unwrap();
        }
        tx_mut.commit().unwrap();
    }

    #[test]
    fn sanity_path() {
        //let genesis
        let (mut block1, exec1) = blocks::block1();
        block1.block.header.header.number = 11;
        block1.block.header.header.state_root =
            H256(hex!("5d035ccb3e75a9057452ff060b773b213ec1fc353426174068edfc3971a0b6bd"));
        let (mut block2, exec2) = blocks::block2();
        block2.block.header.header.number = 12;
        block2.block.header.header.state_root =
            H256(hex!("90101a13dd059fa5cca99ed93d1dc23657f63626c5b8f993a2ccbdf7446b64f8"));

        // test pops execution results from vector, so order is from last to first.ß
        let externals = externals(vec![exec2.clone(), exec1.clone(), exec2.clone(), exec1.clone()]);

        setup(&externals);
        // last finalized block would be number 9.
        let mut tree = BlockchainTree::new(externals, 1, 2, 3).unwrap();

        // genesis block 10 is already canonical
        assert_eq!(tree.make_canonical(&H256::zero()), Ok(()));

        // insert block2 hits max chain size
        assert_eq!(
            tree.insert_block_with_senders(&block2),
            Err(ExecError::PendingBlockIsInFuture {
                block_number: block2.number,
                block_hash: block2.hash(),
                last_finalized: 9,
            }
            .into())
        );

        // make genesis block 10 as finalized
        tree.finalize_block(10);

        // block 2 parent is not known.
        assert_eq!(tree.insert_block_with_senders(&block2), Ok(false));

        // insert block1
        assert_eq!(tree.insert_block_with_senders(&block1), Ok(true));
        // already inserted block will return true.
        assert_eq!(tree.insert_block_with_senders(&block1), Ok(true));

        // insert block2
        assert_eq!(tree.insert_block_with_senders(&block2), Ok(true));

        // Trie state:
        //      b2 (pending block)
        //      |
        //      |
        //      b1 (pending block)
        //    /
        //  /
        // g1 (canonical blocks)
        // |

        // make block1 canonical
        assert_eq!(tree.make_canonical(&block1.hash()), Ok(()));
        // make block2 canonical
        assert_eq!(tree.make_canonical(&block2.hash()), Ok(()));

        // Trie state:
        // b2 (canonical block)
        // |
        // |
        // b1 (canonical block)
        // |
        // |
        // g1 (canonical blocks)
        // |

        let mut block1a = block1.clone();
        let block1a_hash = H256([0x33; 32]);
        block1a.block.header.hash = block1a_hash;
        let mut block2a = block2.clone();
        let block2a_hash = H256([0x34; 32]);
        block2a.block.header.hash = block2a_hash;

        // reinsert two blocks that point to canonical chain
        assert_eq!(tree.insert_block_with_senders(&block1a), Ok(true));
        assert_eq!(tree.insert_block_with_senders(&block2a), Ok(true));

        // Trie state:
        // b2   b2a (side chain)
        // |   /
        // | /
        // b1  b1a (side chain)
        // |  /
        // |/
        // g1 (10)
        // |
        assert_eq!(tree.chains.len(), 2);
        assert_eq!(
            tree.block_indices.blocks_to_chain,
            HashMap::from([(block1a_hash, 1), (block2a_hash, 2)])
        );
        assert_eq!(
            tree.block_indices.fork_to_child,
            HashMap::from([
                (block1.parent_hash, HashSet::from([block1a_hash])),
                (block1.hash(), HashSet::from([block2a_hash]))
            ])
        );

        // make b2a canonical
        assert_eq!(tree.make_canonical(&block2a_hash), Ok(()));
        // Trie state:
        // b2a   b2 (side chain)
        // |   /
        // | /
        // b1  b1a (side chain)
        // |  /
        // |/
        // g1 (10)
        // |

        assert_eq!(tree.make_canonical(&block1a_hash), Ok(()));
        // Trie state:
        //       b2a   b2 (side chain)
        //       |   /
        //       | /
        // b1a  b1 (side chain)
        // |  /
        // |/
        // g1 (10)
        // |

        assert_eq!(tree.chains.len(), 2);
        assert_eq!(
            tree.block_indices.blocks_to_chain,
            HashMap::from([(block1.hash(), 4), (block2a_hash, 4), (block2.hash(), 3)])
        );
        assert_eq!(
            tree.block_indices.fork_to_child,
            HashMap::from([
                (block1.parent_hash, HashSet::from([block1.hash()])),
                (block1.hash(), HashSet::from([block2.hash()]))
            ])
        );

        // make b2 canonical
        assert_eq!(tree.make_canonical(&block2.hash()), Ok(()));
        // Trie state:
        // b2   b2a (side chain)
        // |   /
        // | /
        // b1  b1a (side chain)
        // |  /
        // |/
        // g1 (10)
        // |

        // finalize b1 that would make b1a removed from tree
        tree.finalize_block(11);
        // Trie state:
        // b2   b2a (side chain)
        // |   /
        // | /
        // b1 (canon)
        // |
        // g1 (10)
        // |

        // update canonical block to b2, this would make b2a be removed
        assert_eq!(tree.update_canonical_hashes(12), Ok(()));
        // Trie state:
        // b2 (canon)
        // |
        // b1 (canon)
        // |
        // g1 (10)
        // |
    }
}
