use criterion::{BatchSize, BenchmarkId, Throughput};
use solana_accounts_db::accounts_hash::{AccountHash, AccountsHasher};
use solana_sdk::hash::Hash;
use solana_type_overrides::rand;
use solana_type_overrides::rand::seq::SliceRandom;
use {
    criterion::{criterion_group, criterion_main, Criterion},
    solana_sdk::pubkey::Pubkey,
};

fn bench_accounts_delta_hash(c: &mut Criterion) {
    const ACCOUNTS_COUNTS: [usize; 4] = [
        1,      // the smallest count; will bench overhead
        100,    // number of accounts written per slot on mnb (with *no* rent rewrites)
        1_000,  // number of accounts written slot on mnb (with rent rewrites)
        10_000, // reasonable largest number of accounts written per slot
    ];

    fn create_account_hashes(accounts_count: usize) -> Vec<(Pubkey, AccountHash)> {
        let mut account_hashes: Vec<_> = std::iter::repeat_with(|| {
            let address = Pubkey::new_unique();
            let hash = AccountHash(Hash::new_unique());
            (address, hash)
        })
        .take(accounts_count)
        .collect();

        // since the accounts delta hash needs to sort the accounts first, ensure we're not
        // creating a pre-sorted vec.
        let mut rng = rand::thread_rng();
        account_hashes.shuffle(&mut rng);
        account_hashes
    }

    let mut group = c.benchmark_group("accounts_delta_hash");
    for accounts_count in ACCOUNTS_COUNTS {
        group.throughput(Throughput::Elements(accounts_count as u64));
        let account_hashes = create_account_hashes(accounts_count);
        group.bench_function(BenchmarkId::new("accounts_count", accounts_count), |b| {
            b.iter_batched(
                || account_hashes.clone(),
                AccountsHasher::accumulate_account_hashes,
                BatchSize::SmallInput,
            );
        });
    }
}

criterion_group!(benches, bench_accounts_delta_hash);
criterion_main!(benches);
