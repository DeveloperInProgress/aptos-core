// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use aptos_config::config::NodeConfig;
use aptos_crypto::hash::HashValue;
use aptos_mempool::mocks::MockSharedMempool;
use aptos_sdk::{
    transaction_builder::TransactionFactory,
    types::{account_config::aptos_root_address, transaction::SignedTransaction, LocalAccount},
};
use aptos_temppath::TempPath;
use aptos_types::{
    account_address::AccountAddress,
    block_info::BlockInfo,
    block_metadata::BlockMetadata,
    chain_id::ChainId,
    ledger_info::{LedgerInfo, LedgerInfoWithSignatures},
    transaction::{Transaction, TransactionStatus},
};
use aptos_vm::AptosVM;
use aptosdb::AptosDB;
use executor::{block_executor::BlockExecutor, db_bootstrapper};
use executor_types::BlockExecutorTrait;
use mempool_notifications::MempoolNotificationSender;
use storage_interface::DbReaderWriter;

use aptos_api::context::Context;
use aptos_api_types::TransactionOnChainData;
use aptos_config::keys::ConfigKey;
use aptos_crypto::ed25519::Ed25519PrivateKey;
use rand::SeedableRng;
use std::{boxed::Box, collections::BTreeMap, iter::once, sync::Arc};
use storage_interface::state_view::DbStateView;
use vm_validator::vm_validator::VMValidator;

#[allow(dead_code)]
pub fn new_test_context(test_name: &'static str) -> TestContext {
    let tmp_dir = TempPath::new();
    tmp_dir.create_as_dir().unwrap();

    let mut rng = ::rand::rngs::StdRng::from_seed([0u8; 32]);
    let builder = aptos_genesis::builder::Builder::new(
        tmp_dir.path(),
        cached_framework_packages::module_blobs().to_vec(),
    )
    .unwrap()
    .with_min_price_per_gas_unit(0)
    .with_min_lockup_duration_secs(0)
    .with_max_lockup_duration_secs(86400)
    .with_initial_lockup_timestamp(0)
    .with_randomize_first_validator_ports(false);

    let (root_key, genesis, genesis_waypoint, validators) = builder.build(&mut rng).unwrap();
    let (validator_identity, _, _) = validators[0].get_key_objects(None).unwrap();
    let validator_owner = validator_identity.account_address.unwrap();

    let (db, db_rw) = DbReaderWriter::wrap(AptosDB::new_for_test(&tmp_dir));
    let ret =
        db_bootstrapper::maybe_bootstrap::<AptosVM>(&db_rw, &genesis, genesis_waypoint).unwrap();
    assert!(ret);

    let mempool = MockSharedMempool::new_in_runtime(&db_rw, VMValidator::new(db.clone()));

    TestContext::new(
        Context::new(
            ChainId::test(),
            db.clone(),
            mempool.ac_client.clone(),
            NodeConfig::default(),
        ),
        rng,
        root_key,
        validator_owner,
        Box::new(BlockExecutor::<AptosVM>::new(db_rw)),
        mempool,
        db,
        test_name,
    )
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct TestContext {
    pub context: Context,
    pub validator_owner: AccountAddress,
    pub mempool: Arc<MockSharedMempool>,
    pub db: Arc<AptosDB>,
    rng: rand::rngs::StdRng,
    root_key: ConfigKey<Ed25519PrivateKey>,
    executor: Arc<dyn BlockExecutorTrait>,
    expect_status_code: u16,
    test_name: &'static str,
    fake_time: u64,
}

#[allow(dead_code)]
impl TestContext {
    pub fn new(
        context: Context,
        rng: rand::rngs::StdRng,
        root_key: Ed25519PrivateKey,
        validator_owner: AccountAddress,
        executor: Box<dyn BlockExecutorTrait>,
        mempool: MockSharedMempool,
        db: Arc<AptosDB>,
        test_name: &'static str,
    ) -> Self {
        Self {
            context,
            rng,
            root_key: ConfigKey::new(root_key),
            validator_owner,
            executor: executor.into(),
            mempool: Arc::new(mempool),
            expect_status_code: 200,
            db,
            test_name,
            fake_time: 0,
        }
    }
    pub fn rng(&mut self) -> &mut rand::rngs::StdRng {
        &mut self.rng
    }

    pub fn transaction_factory(&self) -> TransactionFactory {
        TransactionFactory::new(self.context.chain_id())
    }

    pub fn root_account(&self) -> LocalAccount {
        LocalAccount::new(aptos_root_address(), self.root_key.private_key(), 0)
    }

    pub fn latest_state_view(&self) -> DbStateView {
        self.context
            .state_view_at_version(self.get_latest_ledger_info().version())
            .unwrap()
    }

    pub fn gen_account(&mut self) -> LocalAccount {
        LocalAccount::generate(self.rng())
    }

    pub fn create_user_account(&self, account: &LocalAccount) -> SignedTransaction {
        let mut tc = self.root_account();
        self.create_user_account_by(&mut tc, account)
    }

    pub fn create_user_account_by(
        &self,
        creator: &mut LocalAccount,
        account: &LocalAccount,
    ) -> SignedTransaction {
        let factory = self.transaction_factory();
        creator.sign_with_transaction_builder(
            factory
                .create_user_account(account.public_key())
                .expiration_timestamp_secs(u64::MAX),
        )
    }

    pub fn get_latest_ledger_info(&self) -> aptos_api_types::LedgerInfo {
        self.context.get_latest_ledger_info().unwrap()
    }

    pub fn get_transactions(&self, start: u64, limit: u16) -> Vec<TransactionOnChainData> {
        self.context
            .get_transactions(start, limit, self.get_latest_ledger_info().version())
            .unwrap()
    }

    pub fn expect_status_code(&self, status_code: u16) -> TestContext {
        let mut ret = self.clone();
        ret.expect_status_code = status_code;
        ret
    }

    pub async fn commit_mempool_txns(&mut self, size: u64) {
        let txns = self.mempool.get_txns(size);
        self.commit_block(&txns).await;
        for txn in txns {
            self.mempool.remove_txn(&txn);
        }
    }

    pub async fn commit_block(&mut self, signed_txns: &[SignedTransaction]) {
        let metadata = self.new_block_metadata();
        let timestamp = metadata.timestamp_usecs();
        let txns: Vec<Transaction> = std::iter::once(Transaction::BlockMetadata(metadata.clone()))
            .chain(
                signed_txns
                    .iter()
                    .cloned()
                    .map(Transaction::UserTransaction),
            )
            .chain(once(Transaction::StateCheckpoint(metadata.id())))
            .collect();

        // Check that txn execution was successful.
        let parent_id = self.executor.committed_block_id();
        let result = self
            .executor
            .execute_block((metadata.id(), txns.clone()), parent_id)
            .unwrap();
        let mut compute_status = result.compute_status().clone();
        assert_eq!(compute_status.len(), txns.len(), "{:?}", result);
        if matches!(compute_status.last(), Some(TransactionStatus::Retry)) {
            // a state checkpoint txn can be Retry if prefixed by a write set txn
            compute_status.pop();
        }
        // But the rest of the txns must be Kept.
        for st in result.compute_status() {
            match st {
                TransactionStatus::Discard(st) => panic!("transaction is discarded: {:?}", st),
                TransactionStatus::Retry => panic!("should not retry"),
                TransactionStatus::Keep(_) => (),
            }
        }

        self.executor
            .commit_blocks(
                vec![metadata.id()],
                self.new_ledger_info(&metadata, result.root_hash(), txns.len()),
            )
            .unwrap();

        self.mempool
            .mempool_notifier
            .notify_new_commit(txns, timestamp, 1000)
            .await
            .unwrap();
    }

    fn new_block_metadata(&mut self) -> BlockMetadata {
        let round = 1;
        let id = HashValue::random_with_rng(&mut self.rng);
        self.fake_time += 1;
        let timestamp = self.fake_time;
        BlockMetadata::new(
            id,
            0,
            round,
            vec![false],
            self.validator_owner,
            vec![],
            timestamp,
        )
    }

    fn new_ledger_info(
        &self,
        metadata: &BlockMetadata,
        root_hash: HashValue,
        block_size: usize,
    ) -> LedgerInfoWithSignatures {
        let parent = self
            .context
            .get_latest_ledger_info_with_signatures()
            .unwrap();
        let epoch = parent.ledger_info().epoch();
        let version = parent.ledger_info().version() + (block_size as u64);
        let info = LedgerInfo::new(
            BlockInfo::new(
                epoch,
                metadata.round(),
                metadata.id(),
                root_hash,
                version,
                metadata.timestamp_usecs(),
                None,
            ),
            HashValue::zero(),
        );
        LedgerInfoWithSignatures::new(info, BTreeMap::new())
    }
}
