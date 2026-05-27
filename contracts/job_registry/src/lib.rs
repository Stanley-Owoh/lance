#![no_std]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, log, panic_with_error, symbol_short,
    Address, Bytes, Env, Vec,
};

const MAX_CID_LEN: u32 = 96;

#[contracterror]
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum JobRegistryError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    InvalidJobId = 3,
    InvalidBudget = 4,
    InvalidHash = 5,
    JobAlreadyExists = 6,
    JobNotFound = 7,
    JobNotOpen = 8,
    Unauthorized = 9,
    BidAlreadySubmitted = 10,
    BidNotFound = 11,
    InvalidStateTransition = 12,
    NoDeliverable = 13,
    Overflow = 14,
    BidIndexOutOfBounds = 15,
    InvalidCollateral = 16,
    NoRefund = 17,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum JobStatus {
    Open,
    Assigned,
    InProgress,
    DeliverableSubmitted,
    Completed,
    Disputed,
}

#[contracttype]
#[derive(Clone)]
pub struct JobRecord {
    pub client: Address,
    pub freelancer: Option<Address>,
    pub metadata_hash: Bytes,
    pub budget_stroops: i128,
    pub status: JobStatus,
}

#[contracttype]
#[derive(Clone)]
pub struct BidRecord {
    pub freelancer: Address,
    pub proposal_hash: Bytes,
    pub collateral_stroops: i128,
}

#[contracttype]
pub enum DataKey {
    Admin,
    NextJobId,
    Job(u64),
    BidCount(u64),
    Bid(u64, u32),
    BidIndex(u64, Address),
    Refund(Address),
    Deliverable(u64),
}

#[contract]
pub struct JobRegistryContract;

#[contractimpl]
impl JobRegistryContract {
    /// One-time storage bootstrap.
    ///
    /// Sets contract admin and initializes `next_job_id` to 1.
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic_with_error!(&env, JobRegistryError::AlreadyInitialized);
        }

        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::NextJobId, &1u64);

        log!(&env, "JobRegistry initialized with admin: {}", admin);
        env.events().publish((symbol_short!("init"),), admin);
    }

    /// Returns whether storage has been initialized.
    pub fn is_initialized(env: Env) -> bool {
        env.storage().instance().has(&DataKey::Admin)
    }

    pub fn get_admin(env: Env) -> Address {
        read_admin(&env)
    }

    pub fn get_next_job_id(env: Env) -> u64 {
        read_next_job_id(&env)
    }

    /// Client posts a job with explicit `job_id`.
    /// `metadata_hash` must contain compact IPFS CID bytes, not raw text.
    pub fn post_job(env: Env, job_id: u64, client: Address, hash: Bytes, budget: i128) {
        ensure_initialized(&env);
        validate_job_input(&env, job_id, &hash, budget);

        client.require_auth();
        post_job_with_id(&env, job_id, client.clone(), hash, budget);

        // Keep auto-id monotonic when explicit ids are used.
        let next_job_id = read_next_job_id(&env);
        if job_id >= next_job_id {
            let updated = job_id
                .checked_add(1)
                .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::Overflow));
            env.storage().instance().set(&DataKey::NextJobId, &updated);
        }

        log!(
            &env,
            "post_job: id {} client {} budget {}",
            job_id,
            client,
            budget
        );
        env.events()
            .publish((symbol_short!("jobpost"), job_id), (client, budget));
    }

    /// Client posts a job using internal registry index allocation.
    pub fn post_job_auto(env: Env, client: Address, hash: Bytes, budget: i128) -> u64 {
        ensure_initialized(&env);

        let job_id = read_next_job_id(&env);
        validate_job_input(&env, job_id, &hash, budget);

        client.require_auth();
        post_job_with_id(&env, job_id, client.clone(), hash, budget);

        let next = job_id
            .checked_add(1)
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::Overflow));
        env.storage().instance().set(&DataKey::NextJobId, &next);

        log!(
            &env,
            "post_job_auto: id {} client {} budget {}",
            job_id,
            client,
            budget
        );
        env.events()
            .publish((symbol_short!("jobauto"), job_id), (client, budget));

        job_id
    }

    /// Freelancer submits a bid with compact IPFS CID proposal metadata.
    pub fn submit_bid(env: Env, job_id: u64, freelancer: Address, proposal_hash: Bytes) {
        submit_bid_with_collateral_internal(&env, job_id, freelancer, proposal_hash, 0i128);
    }

    /// Freelancer submits a bid with collateral tracked for later refund.
    pub fn submit_bid_with_collateral(
        env: Env,
        job_id: u64,
        freelancer: Address,
        proposal_hash: Bytes,
        collateral_stroops: i128,
    ) {
        submit_bid_with_collateral_internal(
            &env,
            job_id,
            freelancer,
            proposal_hash,
            collateral_stroops,
        );
    }

    /// Freelancer cancels an open bid and credits collateral for refund.
    pub fn cancel_bid(env: Env, job_id: u64, freelancer: Address) -> i128 {
        ensure_initialized(&env);
        freelancer.require_auth();

        let job = read_job(&env, job_id);
        if job.status != JobStatus::Open {
            panic_with_error!(&env, JobRegistryError::JobNotOpen);
        }

        let bidder_key = DataKey::BidIndex(job_id, freelancer.clone());
        let bid_index: u32 = env
            .storage()
            .persistent()
            .get(&bidder_key)
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::BidNotFound));
        let bid = read_bid_at(&env, job_id, bid_index);
        let bid_count = read_bid_count(&env, job_id);
        let last_index = bid_count
            .checked_sub(1)
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::BidIndexOutOfBounds));

        if bid_index != last_index {
            let moved_bid = read_bid_at(&env, job_id, last_index);
            env.storage()
                .persistent()
                .set(&DataKey::Bid(job_id, bid_index), &moved_bid);
            env.storage().persistent().set(
                &DataKey::BidIndex(job_id, moved_bid.freelancer.clone()),
                &bid_index,
            );
        }

        env.storage()
            .persistent()
            .remove(&DataKey::Bid(job_id, last_index));
        env.storage().persistent().remove(&bidder_key);
        env.storage()
            .persistent()
            .set(&DataKey::BidCount(job_id), &last_index);
        credit_refund(&env, freelancer.clone(), bid.collateral_stroops);

        log!(
            &env,
            "cancel_bid: id {} freelancer {} collateral {}",
            job_id,
            freelancer,
            bid.collateral_stroops
        );
        env.events()
            .publish((symbol_short!("bidcan"), job_id), (freelancer, bid.collateral_stroops));

        bid.collateral_stroops
    }

    /// Freelancer claims accumulated collateral credited by cancelled bids.
    pub fn claim_refund(env: Env, freelancer: Address) -> i128 {
        ensure_initialized(&env);
        freelancer.require_auth();

        let key = DataKey::Refund(freelancer.clone());
        let amount = read_refund_balance(&env, freelancer.clone());
        if amount <= 0 {
            panic_with_error!(&env, JobRegistryError::NoRefund);
        }

        env.storage().persistent().remove(&key);
        log!(&env, "claim_refund: freelancer {} amount {}", freelancer, amount);
        env.events()
            .publish((symbol_short!("refund"),), (freelancer, amount));

        amount
    }

    pub fn get_refund_balance(env: Env, freelancer: Address) -> i128 {
        ensure_initialized(&env);
        read_refund_balance(&env, freelancer)
    }

    /// Client accepts a bid, locking in the freelancer.
    pub fn accept_bid(env: Env, job_id: u64, client: Address, freelancer: Address) {
        ensure_initialized(&env);
        client.require_auth();

        let key = DataKey::Job(job_id);
        let mut job = read_job(&env, job_id);

        if job.status != JobStatus::Open {
            panic_with_error!(&env, JobRegistryError::JobNotOpen);
        }
        if client != job.client {
            panic_with_error!(&env, JobRegistryError::Unauthorized);
        }

        let selected_bid_index: u32 = env
            .storage()
            .persistent()
            .get(&DataKey::BidIndex(job_id, freelancer.clone()))
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::BidNotFound));
        let selected_bid = read_bid_at(&env, job_id, selected_bid_index);
        refund_losing_bids(&env, job_id, freelancer.clone(), selected_bid.clone());

        job.freelancer = Some(freelancer.clone());
        job.status = JobStatus::Assigned;
        env.storage().persistent().set(&key, &job);

        log!(
            &env,
            "accept_bid: id {} client {} freelancer {}",
            job_id,
            client,
            freelancer
        );
        env.events()
            .publish((symbol_short!("accept"), job_id), freelancer);
    }

    /// Freelancer submits a deliverable CID.
    pub fn submit_deliverable(env: Env, job_id: u64, freelancer: Address, hash: Bytes) {
        ensure_initialized(&env);
        validate_cid(&env, &hash);
        freelancer.require_auth();

        let key = DataKey::Job(job_id);
        let mut job = read_job(&env, job_id);

        if job.status != JobStatus::Assigned && job.status != JobStatus::InProgress {
            panic_with_error!(&env, JobRegistryError::InvalidStateTransition);
        }
        if job.freelancer != Some(freelancer.clone()) {
            panic_with_error!(&env, JobRegistryError::Unauthorized);
        }

        job.status = JobStatus::DeliverableSubmitted;
        env.storage().persistent().set(&key, &job);
        env.storage()
            .persistent()
            .set(&DataKey::Deliverable(job_id), &hash);

        log!(
            &env,
            "submit_deliverable: id {} freelancer {}",
            job_id,
            freelancer
        );
        env.events()
            .publish((symbol_short!("deliver"), job_id), freelancer);
    }

    /// Mark job disputed. Only the initialized admin can call this.
    pub fn mark_disputed(env: Env, job_id: u64) {
        ensure_initialized(&env);
        let admin = read_admin(&env);
        admin.require_auth();

        let key = DataKey::Job(job_id);
        let mut job = read_job(&env, job_id);

        if job.status != JobStatus::Assigned
            && job.status != JobStatus::InProgress
            && job.status != JobStatus::DeliverableSubmitted
        {
            panic_with_error!(&env, JobRegistryError::InvalidStateTransition);
        }

        job.status = JobStatus::Disputed;
        env.storage().persistent().set(&key, &job);

        log!(&env, "mark_disputed: id {}", job_id);
        env.events().publish((symbol_short!("dispute"), job_id), ());
    }

    pub fn get_job(env: Env, job_id: u64) -> JobRecord {
        ensure_initialized(&env);
        read_job(&env, job_id)
    }

    pub fn get_bids(env: Env, job_id: u64) -> Vec<BidRecord> {
        ensure_initialized(&env);
        read_job(&env, job_id);

        let bid_count = read_bid_count(&env, job_id);
        let mut bids = Vec::new(&env);
        let mut index = 0u32;
        while index < bid_count {
            bids.push_back(read_bid_at(&env, job_id, index));
            index += 1;
        }
        bids
    }

    pub fn get_bid_at(env: Env, job_id: u64, index: u32) -> BidRecord {
        ensure_initialized(&env);
        read_job(&env, job_id);
        let bid_count = read_bid_count(&env, job_id);
        if index >= bid_count {
            panic_with_error!(&env, JobRegistryError::BidIndexOutOfBounds);
        }
        read_bid_at(&env, job_id, index)
    }

    pub fn get_deliverable(env: Env, job_id: u64) -> Bytes {
        ensure_initialized(&env);
        env.storage()
            .persistent()
            .get(&DataKey::Deliverable(job_id))
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::NoDeliverable))
    }
}

fn ensure_initialized(env: &Env) {
    if !env.storage().instance().has(&DataKey::Admin) {
        panic_with_error!(env, JobRegistryError::NotInitialized);
    }
}

fn read_admin(env: &Env) -> Address {
    ensure_initialized(env);
    env.storage()
        .instance()
        .get(&DataKey::Admin)
        .unwrap_or_else(|| panic_with_error!(env, JobRegistryError::NotInitialized))
}

fn read_next_job_id(env: &Env) -> u64 {
    ensure_initialized(env);
    env.storage()
        .instance()
        .get(&DataKey::NextJobId)
        .unwrap_or_else(|| panic_with_error!(env, JobRegistryError::NotInitialized))
}

fn validate_job_input(env: &Env, job_id: u64, hash: &Bytes, budget: i128) {
    if job_id == 0 {
        panic_with_error!(env, JobRegistryError::InvalidJobId);
    }
    if budget <= 0 {
        panic_with_error!(env, JobRegistryError::InvalidBudget);
    }
    validate_cid(env, hash);
}

fn validate_cid(env: &Env, cid: &Bytes) {
    let len = cid.len();
    if len == 0 || len > MAX_CID_LEN {
        panic_with_error!(env, JobRegistryError::InvalidHash);
    }
}

fn validate_collateral(env: &Env, collateral_stroops: i128) {
    if collateral_stroops < 0 {
        panic_with_error!(env, JobRegistryError::InvalidCollateral);
    }
}

fn submit_bid_with_collateral_internal(
    env: &Env,
    job_id: u64,
    freelancer: Address,
    proposal_hash: Bytes,
    collateral_stroops: i128,
) {
    ensure_initialized(env);
    validate_cid(env, &proposal_hash);
    validate_collateral(env, collateral_stroops);
    freelancer.require_auth();

    let job = read_job(env, job_id);
    if job.status != JobStatus::Open {
        panic_with_error!(env, JobRegistryError::JobNotOpen);
    }

    let bidder_key = DataKey::BidIndex(job_id, freelancer.clone());
    if env.storage().persistent().has(&bidder_key) {
        panic_with_error!(env, JobRegistryError::BidAlreadySubmitted);
    }

    let bid_count = read_bid_count(env, job_id);
    let next_count = bid_count
        .checked_add(1)
        .unwrap_or_else(|| panic_with_error!(env, JobRegistryError::Overflow));
    let bid = BidRecord {
        freelancer: freelancer.clone(),
        proposal_hash,
        collateral_stroops,
    };

    // Store bid rows independently so duplicate checks, cancellation, and
    // acceptance avoid deserializing an ever-growing bid vector on write paths.
    env.storage()
        .persistent()
        .set(&DataKey::Bid(job_id, bid_count), &bid);
    env.storage().persistent().set(&bidder_key, &bid_count);
    env.storage()
        .persistent()
        .set(&DataKey::BidCount(job_id), &next_count);

    log!(
        &env,
        "submit_bid: id {} freelancer {} collateral {}",
        job_id,
        freelancer,
        collateral_stroops
    );
    env.events()
        .publish((symbol_short!("bid"), job_id), (freelancer, collateral_stroops));
}

fn read_refund_balance(env: &Env, freelancer: Address) -> i128 {
    env.storage()
        .persistent()
        .get(&DataKey::Refund(freelancer))
        .unwrap_or(0i128)
}

fn credit_refund(env: &Env, freelancer: Address, amount: i128) {
    if amount == 0 {
        return;
    }

    let current = read_refund_balance(env, freelancer.clone());
    let updated = current
        .checked_add(amount)
        .unwrap_or_else(|| panic_with_error!(env, JobRegistryError::Overflow));
    env.storage()
        .persistent()
        .set(&DataKey::Refund(freelancer), &updated);
}

fn refund_losing_bids(
    env: &Env,
    job_id: u64,
    accepted_freelancer: Address,
    accepted_bid: BidRecord,
) {
    let bid_count = read_bid_count(env, job_id);
    let mut index = 0u32;
    while index < bid_count {
        let bid = read_bid_at(env, job_id, index);
        if bid.freelancer != accepted_freelancer {
            credit_refund(env, bid.freelancer.clone(), bid.collateral_stroops);
            env.storage()
                .persistent()
                .remove(&DataKey::BidIndex(job_id, bid.freelancer));
        }
        env.storage()
            .persistent()
            .remove(&DataKey::Bid(job_id, index));
        index += 1;
    }

    env.storage()
        .persistent()
        .set(&DataKey::Bid(job_id, 0u32), &accepted_bid);
    env.storage()
        .persistent()
        .set(&DataKey::BidIndex(job_id, accepted_freelancer), &0u32);
    env.storage()
        .persistent()
        .set(&DataKey::BidCount(job_id), &1u32);
}

fn read_job(env: &Env, job_id: u64) -> JobRecord {
    env.storage()
        .persistent()
        .get(&DataKey::Job(job_id))
        .unwrap_or_else(|| panic_with_error!(env, JobRegistryError::JobNotFound))
}

fn read_bid_count(env: &Env, job_id: u64) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::BidCount(job_id))
        .unwrap_or(0u32)
}

fn read_bid_at(env: &Env, job_id: u64, index: u32) -> BidRecord {
    env.storage()
        .persistent()
        .get(&DataKey::Bid(job_id, index))
        .unwrap_or_else(|| panic_with_error!(env, JobRegistryError::BidIndexOutOfBounds))
}

fn post_job_with_id(env: &Env, job_id: u64, client: Address, hash: Bytes, budget: i128) {
    let key = DataKey::Job(job_id);
    if env.storage().persistent().has(&key) {
        panic_with_error!(env, JobRegistryError::JobAlreadyExists);
    }

    let job = JobRecord {
        client,
        freelancer: None,
        metadata_hash: hash,
        budget_stroops: budget,
        status: JobStatus::Open,
    };
    env.storage().persistent().set(&key, &job);

    env.storage()
        .persistent()
        .set(&DataKey::BidCount(job_id), &0u32);
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::{Address, Bytes, Env};

    fn setup() -> (
        Env,
        JobRegistryContractClient<'static>,
        Address,
        Address,
        Address,
    ) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let client = Address::generate(&env);
        let freelancer = Address::generate(&env);

        let contract_id = env.register_contract(None, JobRegistryContract);
        let cc = JobRegistryContractClient::new(&env, &contract_id);

        (env, cc, admin, client, freelancer)
    }

    #[test]
    fn test_initialize_bootstraps_storage() {
        let (_env, cc, admin, _, _) = setup();

        cc.initialize(&admin);

        assert!(cc.is_initialized());
        assert_eq!(cc.get_admin(), admin);
        assert_eq!(cc.get_next_job_id(), 1u64);
    }

    #[test]
    #[should_panic]
    fn test_double_initialize_panics() {
        let (_env, cc, admin, _, _) = setup();

        cc.initialize(&admin);
        cc.initialize(&admin);
    }

    #[test]
    #[should_panic]
    fn test_post_job_before_initialize_panics() {
        let (env, cc, _admin, client, _) = setup();
        let hash = Bytes::from_slice(&env, b"QmHash");
        cc.post_job(&1u64, &client, &hash, &5000i128);
    }

    #[test]
    fn test_post_job_auto_allocates_sequential_ids() {
        let (env, cc, admin, client, _) = setup();
        cc.initialize(&admin);

        let hash1 = Bytes::from_slice(&env, b"QmHash1");
        let hash2 = Bytes::from_slice(&env, b"QmHash2");

        let id1 = cc.post_job_auto(&client, &hash1, &5000i128);
        let id2 = cc.post_job_auto(&client, &hash2, &7000i128);

        assert_eq!(id1, 1u64);
        assert_eq!(id2, 2u64);
        assert_eq!(cc.get_next_job_id(), 3u64);
    }

    #[test]
    fn test_post_job_with_explicit_id_updates_next_job_id() {
        let (env, cc, admin, client, _) = setup();
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"QmHash");
        cc.post_job(&42u64, &client, &hash, &5000i128);

        assert_eq!(cc.get_next_job_id(), 43u64);
    }

    #[test]
    #[should_panic]
    fn test_invalid_budget_panics() {
        let (env, cc, admin, client, _) = setup();
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"QmHash");
        cc.post_job(&1u64, &client, &hash, &0i128);
    }

    #[test]
    #[should_panic]
    fn test_empty_hash_panics() {
        let (env, cc, admin, client, _) = setup();
        cc.initialize(&admin);

        let empty = Bytes::from_slice(&env, b"");
        cc.post_job(&1u64, &client, &empty, &5000i128);
    }

    #[test]
    fn test_full_lifecycle() {
        let (env, cc, admin, client, freelancer) = setup();
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"QmSomeIPFSHash");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        let job = cc.get_job(&1u64);
        assert_eq!(job.status, JobStatus::Open);
        assert_eq!(job.freelancer, None);

        let proposal = Bytes::from_slice(&env, b"QmProposalHash");
        cc.submit_bid(&1u64, &freelancer, &proposal);

        let bids = cc.get_bids(&1u64);
        assert_eq!(bids.len(), 1);

        cc.accept_bid(&1u64, &client, &freelancer);
        let job = cc.get_job(&1u64);
        assert_eq!(job.status, JobStatus::Assigned);
        assert_eq!(job.freelancer, Some(freelancer.clone()));

        let deliverable = Bytes::from_slice(&env, b"QmDeliverableHash");
        cc.submit_deliverable(&1u64, &freelancer, &deliverable);

        let job = cc.get_job(&1u64);
        assert_eq!(job.status, JobStatus::DeliverableSubmitted);

        let d = cc.get_deliverable(&1u64);
        assert_eq!(d, deliverable);
    }

    #[test]
    #[should_panic]
    fn test_duplicate_bid_panics() {
        let (env, cc, admin, client, freelancer) = setup();
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"QmHash");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        let proposal = Bytes::from_slice(&env, b"QmProposal");
        cc.submit_bid(&1u64, &freelancer, &proposal);
        cc.submit_bid(&1u64, &freelancer, &proposal);
    }

    #[test]
    fn test_get_bid_at_reads_indexed_bid_rows() {
        let (env, cc, admin, client, freelancer) = setup();
        let second_freelancer = Address::generate(&env);
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"bafyJobCid");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        let proposal_one = Bytes::from_slice(&env, b"bafyProposalOne");
        let proposal_two = Bytes::from_slice(&env, b"bafyProposalTwo");
        cc.submit_bid(&1u64, &freelancer, &proposal_one);
        cc.submit_bid(&1u64, &second_freelancer, &proposal_two);

        let first = cc.get_bid_at(&1u64, &0u32);
        let second = cc.get_bid_at(&1u64, &1u32);
        assert_eq!(first.freelancer, freelancer);
        assert_eq!(first.proposal_hash, proposal_one);
        assert_eq!(second.freelancer, second_freelancer);
        assert_eq!(second.proposal_hash, proposal_two);

        let bids = cc.get_bids(&1u64);
        assert_eq!(bids.len(), 2);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #15)")]
    fn test_get_bid_at_out_of_bounds_returns_specific_error() {
        let (env, cc, admin, client, _) = setup();
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"bafyJobCid");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        cc.get_bid_at(&1u64, &0u32);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #5)")]
    fn test_rejects_oversized_metadata_cid() {
        let (env, cc, admin, client, _) = setup();
        cc.initialize(&admin);

        let oversized = Bytes::from_slice(
            &env,
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );
        cc.post_job(&1u64, &client, &oversized, &5000i128);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #8)")]
    fn test_late_bid_after_assignment_returns_specific_error() {
        let (env, cc, admin, client, freelancer) = setup();
        let late_freelancer = Address::generate(&env);
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"bafyJobCid");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        let proposal = Bytes::from_slice(&env, b"bafyProposal");
        cc.submit_bid(&1u64, &freelancer, &proposal);
        cc.accept_bid(&1u64, &client, &freelancer);

        let late_proposal = Bytes::from_slice(&env, b"bafyLateProposal");
        cc.submit_bid(&1u64, &late_freelancer, &late_proposal);
    }

    #[test]
    fn test_cancel_bid_refunds_collateral_and_compacts_indices() {
        let (env, cc, admin, client, freelancer) = setup();
        let second_freelancer = Address::generate(&env);
        let third_freelancer = Address::generate(&env);
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"bafyJobCid");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        let proposal_one = Bytes::from_slice(&env, b"bafyProposalOne");
        let proposal_two = Bytes::from_slice(&env, b"bafyProposalTwo");
        let proposal_three = Bytes::from_slice(&env, b"bafyProposalThree");
        cc.submit_bid_with_collateral(&1u64, &freelancer, &proposal_one, &100i128);
        cc.submit_bid_with_collateral(&1u64, &second_freelancer, &proposal_two, &250i128);
        cc.submit_bid_with_collateral(&1u64, &third_freelancer, &proposal_three, &300i128);

        let refunded = cc.cancel_bid(&1u64, &second_freelancer);
        assert_eq!(refunded, 250i128);
        assert_eq!(cc.get_refund_balance(&second_freelancer), 250i128);

        let bids = cc.get_bids(&1u64);
        assert_eq!(bids.len(), 2);
        assert_eq!(cc.get_bid_at(&1u64, &0u32).freelancer, freelancer);
        assert_eq!(
            cc.get_bid_at(&1u64, &1u32).freelancer,
            third_freelancer.clone()
        );

        cc.accept_bid(&1u64, &client, &third_freelancer);
        let job = cc.get_job(&1u64);
        assert_eq!(job.status, JobStatus::Assigned);
        assert_eq!(job.freelancer, Some(third_freelancer));
    }

    #[test]
    fn test_claim_refund_clears_balance() {
        let (env, cc, admin, client, freelancer) = setup();
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"bafyJobCid");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        let proposal = Bytes::from_slice(&env, b"bafyProposal");
        cc.submit_bid_with_collateral(&1u64, &freelancer, &proposal, &125i128);
        cc.cancel_bid(&1u64, &freelancer);

        assert_eq!(cc.claim_refund(&freelancer), 125i128);
        assert_eq!(cc.get_refund_balance(&freelancer), 0i128);
    }

    #[test]
    fn test_accept_bid_refunds_losing_bid_collateral() {
        let (env, cc, admin, client, freelancer) = setup();
        let winning_freelancer = Address::generate(&env);
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"bafyJobCid");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        let losing_proposal = Bytes::from_slice(&env, b"bafyLosingProposal");
        let winning_proposal = Bytes::from_slice(&env, b"bafyWinningProposal");
        cc.submit_bid_with_collateral(&1u64, &freelancer, &losing_proposal, &100i128);
        cc.submit_bid_with_collateral(
            &1u64,
            &winning_freelancer,
            &winning_proposal,
            &250i128,
        );

        cc.accept_bid(&1u64, &client, &winning_freelancer);

        assert_eq!(cc.get_refund_balance(&freelancer), 100i128);
        assert_eq!(cc.get_refund_balance(&winning_freelancer), 0i128);
        assert_eq!(cc.get_bids(&1u64).len(), 1);
        assert_eq!(
            cc.get_bid_at(&1u64, &0u32).freelancer,
            winning_freelancer
        );
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #11)")]
    fn test_cancel_bid_requires_existing_bid() {
        let (env, cc, admin, client, freelancer) = setup();
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"bafyJobCid");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        cc.cancel_bid(&1u64, &freelancer);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #8)")]
    fn test_cancel_bid_after_assignment_returns_specific_error() {
        let (env, cc, admin, client, freelancer) = setup();
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"bafyJobCid");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        let proposal = Bytes::from_slice(&env, b"bafyProposal");
        cc.submit_bid_with_collateral(&1u64, &freelancer, &proposal, &125i128);
        cc.accept_bid(&1u64, &client, &freelancer);
        cc.cancel_bid(&1u64, &freelancer);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #16)")]
    fn test_submit_bid_rejects_negative_collateral() {
        let (env, cc, admin, client, freelancer) = setup();
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"bafyJobCid");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        let proposal = Bytes::from_slice(&env, b"bafyProposal");
        cc.submit_bid_with_collateral(&1u64, &freelancer, &proposal, &-1i128);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #17)")]
    fn test_claim_refund_requires_balance() {
        let (_env, cc, admin, _, freelancer) = setup();
        cc.initialize(&admin);

        cc.claim_refund(&freelancer);
    }

    #[test]
    #[should_panic]
    fn test_accept_without_matching_bid_panics() {
        let (env, cc, admin, client, freelancer) = setup();
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"QmHash");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        cc.accept_bid(&1u64, &client, &freelancer);
    }

    #[test]
    fn test_mark_disputed_from_in_progress() {
        let (env, cc, admin, client, freelancer) = setup();
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"QmHash");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        let proposal = Bytes::from_slice(&env, b"QmProposal");
        cc.submit_bid(&1u64, &freelancer, &proposal);
        cc.accept_bid(&1u64, &client, &freelancer);

        cc.mark_disputed(&1u64);
        let job = cc.get_job(&1u64);
        assert_eq!(job.status, JobStatus::Disputed);
    }

    #[test]
    #[should_panic]
    fn test_mark_disputed_from_open_panics() {
        let (env, cc, admin, client, _) = setup();
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"QmHash");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        cc.mark_disputed(&1u64);
    }

    #[test]
    #[should_panic]
    fn test_get_deliverable_without_submission_panics() {
        let (env, cc, admin, client, _) = setup();
        cc.initialize(&admin);

        let hash = Bytes::from_slice(&env, b"QmHash");
        cc.post_job(&1u64, &client, &hash, &5000i128);

        cc.get_deliverable(&1u64);
    }
}
