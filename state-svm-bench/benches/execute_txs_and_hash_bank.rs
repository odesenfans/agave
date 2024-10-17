use bincode::serialize_into;
use criterion::{BatchSize, BenchmarkId, Throughput};
use jmt::mock::MockTreeStore;
use solana_accounts_db::accounts::Accounts;
use solana_accounts_db::accounts_db::test_utils::create_test_accounts;
use solana_accounts_db::accounts_db::{
    AccountShrinkThreshold, AccountsDb, VerifyAccountsHashAndLamportsConfig,
    ACCOUNTS_DB_CONFIG_FOR_BENCHMARKS,
};
use solana_accounts_db::accounts_hash::{AccountHash, AccountsHasher};
use solana_accounts_db::accounts_index::AccountSecondaryIndexes;
use solana_accounts_db::ancestors::Ancestors;
use solana_runtime::bank::Bank;
use solana_sdk::account::ReadableAccount;
use solana_sdk::clock::Slot;
use solana_sdk::epoch_schedule::EpochSchedule;
use solana_sdk::genesis_config::ClusterType;
use solana_sdk::hash::Hash;
use solana_sdk::rent_collector::RentCollector;
use solana_type_overrides::rand;
use solana_type_overrides::rand::seq::SliceRandom;
use state_svm_bench::mock_bank::{
    create_executable_environment, register_builtins, MockBankCallback, MockForkGraph,
};
use std::cell::RefCell;
use std::path::PathBuf;
use {
    criterion::{criterion_group, criterion_main, Criterion},
    solana_sdk::{
        account::AccountSharedData,
        instruction::AccountMeta,
        keccak::Hasher,
        pubkey::Pubkey,
        signature::Keypair,
        signer::Signer,
        system_instruction, system_program,
        transaction::{SanitizedTransaction, Transaction},
    },
    solana_svm::{
        account_loader::{CheckedTransactionDetails, TransactionCheckResult},
        transaction_processor::{
            ExecutionRecordingConfig, TransactionBatchProcessor, TransactionProcessingConfig,
            TransactionProcessingEnvironment,
        },
    },
    solana_type_overrides::sync::{Arc, RwLock},
    std::collections::HashSet,
};

const NUM_RANDOM_ACCOUNT_KEYS: usize = 12;

fn create_transactions(
    count: usize,
    accounts: &mut Accounts,
    slot: Slot,
) -> Vec<SanitizedTransaction> {
    let mut accounts_to_store = vec![];

    let payer = Keypair::new();
    let payer_account = AccountSharedData::new(100_000_000_000, 0, &system_program::id());
    accounts_to_store.push((payer.pubkey(), payer_account));

    let txs = (0..count)
        .map(|_| {
            let destination = Pubkey::new_unique();
            let destination_account = AccountSharedData::default();
            accounts_to_store.push((destination, destination_account));

            // TODO: use properly random accounts, `new_unique()` generates incremental pubkeys.
            let random_accounts = (0..NUM_RANDOM_ACCOUNT_KEYS)
                .map(|_| (Pubkey::new_unique(), AccountSharedData::default()))
                .collect::<Vec<_>>();
            let random_account_metas = random_accounts.iter().map(|(pubkey, _)| AccountMeta {
                pubkey: *pubkey,
                is_signer: false,
                is_writable: false,
            });
            accounts_to_store.extend_from_slice(&random_accounts);

            let mut ix = system_instruction::transfer(&payer.pubkey(), &destination, 100);
            ix.accounts.extend(random_account_metas);

            let tx = Transaction::new_signed_with_payer(
                &[ix],
                Some(&payer.pubkey()),
                &[&payer],
                solana_sdk::hash::Hash::default(),
            );
            SanitizedTransaction::from_transaction_for_tests(tx)
        })
        .collect();

    let mut unique_keys = HashSet::new();
    for (pubkey, _) in accounts_to_store.iter() {
        if !unique_keys.insert(pubkey) {
            println!("problemo: {}", pubkey.to_string());
        }
    }

    let accounts_to_store_refs: Vec<_> = accounts_to_store
        .iter()
        .map(|(key, account)| (key, account))
        .collect();

    accounts
        .accounts_db
        .store_uncached(slot, accounts_to_store_refs.as_slice());
    // for (i, (pubkey, account_shared_data)) in accounts_to_store.iter().enumerate() {
    //     println!(
    //         "storing account {} ({i}/{})...",
    //         pubkey.to_string(),
    //         accounts_to_store.len()
    //     );
    //     accounts.store_slow_uncached(slot, &pubkey, &account_shared_data);
    // }

    txs
}

fn new_accounts_db(account_paths: Vec<PathBuf>) -> AccountsDb {
    AccountsDb::new_with_config(
        account_paths,
        &ClusterType::Development,
        AccountSecondaryIndexes::default(),
        AccountShrinkThreshold::default(),
        Some(ACCOUNTS_DB_CONFIG_FOR_BENCHMARKS),
        None,
        Arc::default(),
    )
}

fn create_check_results(count: usize) -> Vec<TransactionCheckResult> {
    (0..count)
        .map(|_| {
            TransactionCheckResult::Ok(CheckedTransactionDetails {
                nonce: None,
                lamports_per_signature: 0,
            })
        })
        .collect()
}

fn setup_batch_processor(
    bank: &Bank,
    fork_graph: &Arc<RwLock<MockForkGraph>>,
) -> TransactionBatchProcessor<MockForkGraph> {
    let batch_processor = TransactionBatchProcessor::<MockForkGraph>::new(
        /* slot */ 0,
        /* epoch */ 0,
        HashSet::new(),
    );
    create_executable_environment(
        fork_graph.clone(),
        &bank,
        &mut batch_processor.program_cache.write().unwrap(),
    );
    register_builtins(&bank, &batch_processor);
    batch_processor
}

struct BenchCase<'a> {
    name: &'a str,
    n_accounts_total: usize,
    n_txs: usize,
}

fn execute_txs_and_compute_bank_hash(c: &mut Criterion) {
    let cases = vec![BenchCase {
        name: "Lightweight",
        n_accounts_total: 10_000,
        n_txs: 1,
    }];

    let slot = 0;
    let fork_graph = Arc::new(RwLock::new(MockForkGraph {}));
    let processing_environment = TransactionProcessingEnvironment::default();
    let processing_config = TransactionProcessingConfig {
        recording_config: ExecutionRecordingConfig {
            enable_log_recording: true,         // Record logs
            enable_return_data_recording: true, // Record return data
            enable_cpi_recording: false,        // Don't care about inner instructions.
        },
        ..Default::default()
    };
    let mut group = c.benchmark_group("SVM + bank hashing");

    for case in cases {
        let accounts_db = new_accounts_db(vec![PathBuf::from("bench_solana_behavior")]);
        let mut accounts = Accounts::new(Arc::new(accounts_db));

        let mut pubkeys: Vec<Pubkey> = vec![];
        create_test_accounts(&accounts, &mut pubkeys, case.n_accounts_total, slot);

        let ancestors = Ancestors::from(vec![0]);
        let (_, total_lamports) = accounts
            .accounts_db
            .update_accounts_hash_for_tests(0, &ancestors, false, false);
        accounts.add_root(slot);
        accounts.accounts_db.flush_accounts_cache(true, Some(slot));

        let txs = create_transactions(case.n_txs, &mut accounts, slot);
        let check_results = create_check_results(txs.len());
        let bank = Bank::default_with_accounts(accounts);

        let batch_processor = setup_batch_processor(&bank, &fork_graph);
        group.bench_function(format!("{} Transaction Batch: Control", case.name), |b| {
            b.iter(|| {
                batch_processor.load_and_execute_sanitized_transactions(
                    &bank,
                    &txs,
                    check_results.clone(),
                    &processing_environment,
                    &processing_config,
                );
                bank.freeze();
            })
        });
    }
}
criterion_group!(benches, execute_txs_and_compute_bank_hash);
criterion_main!(benches);
