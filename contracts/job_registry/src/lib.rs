#![no_std]

use soroban_sdk::{contract, contractimpl, contracttype, contracterror, panic_with_error, Address, Bytes, BytesN, Env, Vec};

// ─────────────────────────────────────────────────────────────────────────────
// JobRegistryError – structured error codes for out-of-bounds & invalid states
// ─────────────────────────────────────────────────────────────────────────────
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
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
    JobExpired = 13,
    Overflow = 14,
    BidIndexOutOfBounds = 15,
    ReentrancyDetected = 16,
}

// ─────────────────────────────────────────────────────────────────────────────
// JobStatus – lifecycle states a job can occupy
// ─────────────────────────────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobStatus {
    Open,
    Assigned,
    DeliverableSubmitted,
    Disputed,
    Closed,
}

// ─────────────────────────────────────────────────────────────────────────────
// Core on-chain records – only compact IPFS CIDs are persisted, never raw text
// ─────────────────────────────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobRecord {
    pub client: Address,
    pub freelancer: Option<Address>,
    pub metadata_hash: Bytes,
    pub budget_stroops: i128,
    pub status: JobStatus,
    pub bidding_deadline: u64,
    pub expires_at: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BidRecord {
    pub freelancer: Address,
    pub proposal_hash: Bytes,
}

// ─────────────────────────────────────────────────────────────────────────────
// Storage layout – map-like keys for clean job→bid lookups
// ─────────────────────────────────────────────────────────────────────────────
#[contracttype]
pub enum DataKey {
    Admin,
    Locked,
    UpgradeAdmin,
    Job(u64),
    Bids(u64),
    NextJobId,
}

// ─────────────────────────────────────────────────────────────────────────────
// Event payloads
// ─────────────────────────────────────────────────────────────────────────────
#[contracttype]
#[derive(Clone)]
pub struct BidSubmittedEvent {
    pub job_id: u64,
    pub freelancer: Address,
    pub proposal_hash: Bytes,
    pub timestamp: u64,
}

#[contracttype]
#[derive(Clone)]
pub struct BidAcceptedEvent {
    pub job_id: u64,
    pub client: Address,
    pub freelancer: Address,
    pub timestamp: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Reentrancy guard – prevents reentrant calls during bid modification
// ─────────────────────────────────────────────────────────────────────────────
const MAX_HASH_LEN: u32 = 64;

struct ReentrancyGuard<'a> {
    env: &'a Env,
}

impl Drop for ReentrancyGuard<'_> {
    fn drop(&mut self) {
        self.env.storage().instance().remove(&DataKey::Locked);
    }
}

fn require_not_reentrant(env: &Env) -> ReentrancyGuard<'_> {
    if env.storage().instance().has(&DataKey::Locked) {
        panic_with_error!(env, JobRegistryError::ReentrancyDetected);
    }
    env.storage().instance().set(&DataKey::Locked, &());
    ReentrancyGuard { env }
}

fn hash_is_valid(h: &Bytes) -> bool {
    !h.is_empty() && h.len() <= MAX_HASH_LEN
}

// ─────────────────────────────────────────────────────────────────────────────
// Contract
// ─────────────────────────────────────────────────────────────────────────────
#[contract]
pub struct JobRegistryContract;

#[contractimpl]
impl JobRegistryContract {
    // ── Admin ──────────────────────────────────────────────────────────────
    pub fn initialize(env: Env, admin: Address) {
        if env.storage().instance().has(&DataKey::Admin) {
            panic_with_error!(env, JobRegistryError::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::NextJobId, &1u64);
    }

    pub fn set_upgrade_admin(env: Env, caller: Address, new: Address) {
        caller.require_auth();
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .expect("not initialized");
        if caller != admin {
            panic_with_error!(env, JobRegistryError::Unauthorized);
        }
        env.storage().instance().set(&DataKey::UpgradeAdmin, &new);
    }

    pub fn get_upgrade_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::UpgradeAdmin)
    }

    pub fn upgrade(env: Env, caller: Address, new_wasm_hash: BytesN<32>) {
        caller.require_auth();
        let upgrade_admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::UpgradeAdmin)
            .expect("upgrade admin not set");
        if caller != upgrade_admin {
            panic_with_error!(env, JobRegistryError::Unauthorized);
        }
        env.deployer().update_current_contract_wasm(new_wasm_hash);
    }

    // ── Job posting ───────────────────────────────────────────────────────
    pub fn post_job(
        env: Env,
        job_id: u64,
        client: Address,
        metadata_hash: Bytes,
        budget_stroops: i128,
        bidding_deadline: u64,
        expires_at: u64,
    ) {
        client.require_auth();

        if job_id == 0 {
            panic_with_error!(env, JobRegistryError::InvalidJobId);
        }
        if budget_stroops <= 0 {
            panic_with_error!(env, JobRegistryError::InvalidBudget);
        }
        if !hash_is_valid(&metadata_hash) {
            panic_with_error!(env, JobRegistryError::InvalidHash);
        }

        let job_key = DataKey::Job(job_id);
        if env.storage().persistent().has(&job_key) {
            panic_with_error!(env, JobRegistryError::JobAlreadyExists);
        }

        Self::write_job(&env, job_id, client, metadata_hash, budget_stroops, bidding_deadline, expires_at);

        let mut next: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextJobId)
            .unwrap_or(1);
        if job_id >= next {
            next = job_id.checked_add(1).expect("overflow");
            env.storage().instance().set(&DataKey::NextJobId, &next);
        }
    }

    fn write_job(
        env: &Env,
        job_id: u64,
        client: Address,
        metadata_hash: Bytes,
        budget_stroops: i128,
        bidding_deadline: u64,
        expires_at: u64,
    ) {
        let job = JobRecord {
            client,
            freelancer: None,
            metadata_hash,
            budget_stroops,
            status: JobStatus::Open,
            bidding_deadline,
            expires_at,
        };
        env.storage().persistent().set(&DataKey::Job(job_id), &job);
    }

    pub fn post_job_auto(
        env: Env,
        client: Address,
        metadata_hash: Bytes,
        budget_stroops: i128,
        bidding_deadline: u64,
        expires_at: u64,
    ) -> u64 {
        client.require_auth();

        if budget_stroops <= 0 {
            panic_with_error!(env, JobRegistryError::InvalidBudget);
        }
        if !hash_is_valid(&metadata_hash) {
            panic_with_error!(env, JobRegistryError::InvalidHash);
        }

        let mut next: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextJobId)
            .unwrap_or(1);
        let job_id = next;

        next = next.checked_add(1).expect("overflow");
        env.storage().instance().set(&DataKey::NextJobId, &next);

        Self::write_job(&env, job_id, client, metadata_hash, budget_stroops, bidding_deadline, expires_at);
        job_id
    }

    // ── Bidding ───────────────────────────────────────────────────────────
    pub fn submit_bid(
        env: Env,
        job_id: u64,
        freelancer: Address,
        proposal_hash: Bytes,
        amount: i128,
    ) {
        let _guard = require_not_reentrant(&env);
        freelancer.require_auth();

        if !hash_is_valid(&proposal_hash) {
            panic_with_error!(env, JobRegistryError::InvalidHash);
        }
        if amount < 0 {
            panic_with_error!(env, JobRegistryError::InvalidBudget);
        }

        let now = env.ledger().timestamp();
        let job_key = DataKey::Job(job_id);
        let job: JobRecord = env
            .storage()
            .persistent()
            .get(&job_key)
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::JobNotFound));

        if job.status != JobStatus::Open {
            panic_with_error!(env, JobRegistryError::JobNotOpen);
        }
        if now > job.bidding_deadline && job.bidding_deadline > 0 {
            panic_with_error!(env, JobRegistryError::JobExpired);
        }

        let bids_key = DataKey::Bids(job_id);
        let mut bids: Vec<BidRecord> = env
            .storage()
            .persistent()
            .get(&bids_key)
            .unwrap_or(Vec::new(&env));

        // Prevent duplicate bid from the same freelancer
        for b in bids.iter() {
            if b.freelancer == freelancer {
                panic_with_error!(env, JobRegistryError::BidAlreadySubmitted);
            }
        }

        let ev_freelancer = freelancer.clone();
        let ev_hash = proposal_hash.clone();

        let bid = BidRecord {
            freelancer,
            proposal_hash,
        };
        bids.push_back(bid);

        env.storage().persistent().set(&bids_key, &bids);
        env.storage().persistent().set(&job_key, &job);

        env.events().publish(
            ("job_registry", "BidSubmitted"),
            BidSubmittedEvent {
                job_id,
                freelancer: ev_freelancer,
                proposal_hash: ev_hash,
                timestamp: now,
            },
        );
    }

    pub fn cancel_bid(env: Env, job_id: u64, freelancer: Address) {
        let _guard = require_not_reentrant(&env);
        freelancer.require_auth();

        let job_key = DataKey::Job(job_id);
        let job: JobRecord = env
            .storage()
            .persistent()
            .get(&job_key)
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::JobNotFound));

        if job.status != JobStatus::Open {
            panic_with_error!(env, JobRegistryError::JobNotOpen);
        }

        let bids_key = DataKey::Bids(job_id);
        let mut bids: Vec<BidRecord> = env
            .storage()
            .persistent()
            .get(&bids_key)
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::BidNotFound));

        let len_before = bids.len();
        let mut filtered: Vec<BidRecord> = Vec::new(&env);
        for b in bids.iter() {
            if b.freelancer != freelancer {
                filtered.push_back(b);
            }
        }
        if filtered.len() == len_before {
            panic_with_error!(env, JobRegistryError::BidNotFound);
        }
        bids = filtered;

        env.storage().persistent().set(&bids_key, &bids);
    }

    // ── Acceptance ────────────────────────────────────────────────────────
    pub fn accept_bid(env: Env, job_id: u64, caller: Address, freelancer: Address) {
        let _guard = require_not_reentrant(&env);
        caller.require_auth();

        let now = env.ledger().timestamp();
        let job_key = DataKey::Job(job_id);
        let mut job: JobRecord = env
            .storage()
            .persistent()
            .get(&job_key)
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::JobNotFound));

        // Strict ownership: only the job creator can accept
        if caller != job.client {
            panic_with_error!(env, JobRegistryError::Unauthorized);
        }
        if job.status != JobStatus::Open {
            panic_with_error!(env, JobRegistryError::JobNotOpen);
        }
        if now > job.bidding_deadline && job.bidding_deadline > 0 {
            panic_with_error!(env, JobRegistryError::JobExpired);
        }

        let bids_key = DataKey::Bids(job_id);
        let bids: Vec<BidRecord> = env
            .storage()
            .persistent()
            .get(&bids_key)
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::BidNotFound));

        // Verify the selected freelancer actually submitted a bid
        let mut found = false;
        for b in bids.iter() {
            if b.freelancer == freelancer {
                found = true;
                break;
            }
        }
        if !found {
            panic_with_error!(env, JobRegistryError::BidNotFound);
        }

        let ev_freelancer = freelancer.clone();
        job.status = JobStatus::Assigned;
        job.freelancer = Some(freelancer);
        env.storage().persistent().set(&job_key, &job);

        env.events().publish(
            ("job_registry", "BidAccepted"),
            BidAcceptedEvent {
                job_id,
                client: caller,
                freelancer: ev_freelancer,
                timestamp: now,
            },
        );
    }

    // ── Deliverable ───────────────────────────────────────────────────────
    pub fn submit_deliverable(
        env: Env,
        job_id: u64,
        freelancer: Address,
        deliverable_hash: Bytes,
    ) {
        let _guard = require_not_reentrant(&env);
        freelancer.require_auth();

        if !hash_is_valid(&deliverable_hash) {
            panic_with_error!(env, JobRegistryError::InvalidHash);
        }

        let job_key = DataKey::Job(job_id);
        let mut job: JobRecord = env
            .storage()
            .persistent()
            .get(&job_key)
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::JobNotFound));

        if job.status != JobStatus::Assigned {
            panic_with_error!(env, JobRegistryError::InvalidStateTransition);
        }
        if job.freelancer.as_ref() != Some(&freelancer) {
            panic_with_error!(env, JobRegistryError::Unauthorized);
        }

        job.status = JobStatus::DeliverableSubmitted;
        env.storage().persistent().set(&job_key, &job);
    }

    // ── Dispute ───────────────────────────────────────────────────────────
    pub fn mark_disputed(env: Env, job_id: u64, caller: Address) {
        caller.require_auth();

        let job_key = DataKey::Job(job_id);
        let mut job: JobRecord = env
            .storage()
            .persistent()
            .get(&job_key)
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::JobNotFound));

        // Only client or freelancer can mark disputed
        if caller != job.client && job.freelancer.as_ref() != Some(&caller) {
            panic_with_error!(env, JobRegistryError::Unauthorized);
        }

        match job.status {
            JobStatus::Assigned | JobStatus::DeliverableSubmitted => {
                job.status = JobStatus::Disputed;
                env.storage().persistent().set(&job_key, &job);
            }
            _ => panic_with_error!(env, JobRegistryError::InvalidStateTransition),
        }
    }

    // ── Close / Cancel ────────────────────────────────────────────────────
    pub fn close_job(env: Env, job_id: u64, caller: Address) {
        let _guard = require_not_reentrant(&env);
        caller.require_auth();

        let job_key = DataKey::Job(job_id);
        let mut job: JobRecord = env
            .storage()
            .persistent()
            .get(&job_key)
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::JobNotFound));

        if caller != job.client {
            panic_with_error!(env, JobRegistryError::Unauthorized);
        }

        job.status = JobStatus::Closed;
        env.storage().persistent().set(&job_key, &job);
    }

    pub fn cancel_expired_job(env: Env, job_id: u64, caller: Address) {
        caller.require_auth();

        let job_key = DataKey::Job(job_id);
        let mut job: JobRecord = env
            .storage()
            .persistent()
            .get(&job_key)
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::JobNotFound));

        let now = env.ledger().timestamp();
        if now <= job.expires_at || job.expires_at == 0 {
            panic_with_error!(env, JobRegistryError::InvalidStateTransition);
        }
        if caller != job.client {
            panic_with_error!(env, JobRegistryError::Unauthorized);
        }

        job.status = JobStatus::Closed;
        env.storage().persistent().set(&job_key, &job);
    }

    // ── View functions ────────────────────────────────────────────────────
    pub fn get_job(env: Env, job_id: u64) -> JobRecord {
        env.storage()
            .persistent()
            .get(&DataKey::Job(job_id))
            .unwrap_or_else(|| panic_with_error!(&env, JobRegistryError::JobNotFound))
    }

    pub fn get_job_status(env: Env, job_id: u64) -> JobStatus {
        Self::get_job(env, job_id).status
    }

    pub fn get_bids(env: Env, job_id: u64) -> Vec<BidRecord> {
        // Verify the job exists first
        Self::get_job(env.clone(), job_id);
        env.storage()
            .persistent()
            .get(&DataKey::Bids(job_id))
            .unwrap_or(Vec::new(&env))
    }

    pub fn get_bid_at(env: Env, job_id: u64, index: u32) -> BidRecord {
        let bids = Self::get_bids(env.clone(), job_id);
        if index >= bids.len() {
            panic_with_error!(env, JobRegistryError::BidIndexOutOfBounds);
        }
        bids.get(index).unwrap()
    }

    pub fn get_bids_count(env: Env, job_id: u64) -> u32 {
        Self::get_bids(env.clone(), job_id).len() as u32
    }

    pub fn get_bids_page(
        env: Env,
        job_id: u64,
        offset: u32,
        limit: u32,
    ) -> Vec<BidRecord> {
        let bids = Self::get_bids(env.clone(), job_id);
        let end = (offset + limit).min(bids.len());
        if offset >= bids.len() {
            return Vec::new(&env);
        }
        let mut page = Vec::new(&env);
        let mut i = offset;
        while i < end {
            if let Some(b) = bids.get(i) {
                page.push_back(b);
            }
            i += 1;
        }
        page
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// Tests
// ═════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::{Bytes, Env};

    fn setup_env() -> Env {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|li| {
            li.timestamp = 1_700_000_000;
        });
        env
    }

    fn setup_client(env: &Env) -> JobRegistryContractClient<'_> {
        let contract_id = env.register_contract(None, JobRegistryContract);
        let client = JobRegistryContractClient::new(env, &contract_id);
        let admin = Address::generate(env);
        client.initialize(&admin);
        client
    }

    fn hash(env: &Env, bytes: &[u8]) -> Bytes {
        Bytes::from_slice(env, bytes)
    }

    // ── Initialization ─────────────────────────────────────────────────────
    #[test]
    fn test_initialize_bootstraps_storage() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, JobRegistryContract);
        let client = JobRegistryContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);
    }

    #[test]
    #[should_panic(expected = "Contract, #1")]
    fn test_double_initialize_panics() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, JobRegistryContract);
        let client = JobRegistryContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);
        client.initialize(&admin);
    }

    #[test]
    fn test_post_job_works_without_explicit_initialize() {
        let env = setup_env();
        let contract_id = env.register_contract(None, JobRegistryContract);
        let client = JobRegistryContractClient::new(&env, &contract_id);
        // No explicit initialize – post_job can still create a job
        let owner = Address::generate(&env);
        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        let job = client.get_job(&1);
        assert_eq!(job.client, owner);
        assert_eq!(job.status, JobStatus::Open);
    }

    // ── post_job ───────────────────────────────────────────────────────────
    #[test]
    fn test_post_job_auto_allocates_sequential_ids() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);

        let id1 = client.post_job_auto(&owner, &hash(&env, b"QmHash1"), &1000, &0, &0);
        let id2 = client.post_job_auto(&owner, &hash(&env, b"QmHash2"), &2000, &0, &0);
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
    }

    #[test]
    fn test_post_job_with_explicit_id_updates_next_job_id() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);

        client.post_job(&10, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        // Next should be 11
        let id2 = client.post_job_auto(&owner, &hash(&env, b"QmHash2"), &2000, &0, &0);
        assert_eq!(id2, 11);
    }

    #[test]
    #[should_panic(expected = "Contract, #6")]
    fn test_duplicate_job_id() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        client.post_job(&1, &owner, &hash(&env, b"QmHash1"), &1000, &0, &0);
        client.post_job(&1, &owner, &hash(&env, b"QmHash2"), &2000, &0, &0);
    }

    #[test]
    #[should_panic(expected = "Contract, #4")]
    fn test_invalid_budget_panics() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &0, &0, &0);
    }

    #[test]
    #[should_panic(expected = "Contract, #4")]
    fn test_zero_budget_still_panics() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &0, &0, &0);
    }

    #[test]
    #[should_panic(expected = "Contract, #4")]
    fn test_budget_below_minimum_panics() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &-1, &0, &0);
    }

    #[test]
    fn test_budget_at_minimum_succeeds() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1, &0, &0);
        let job = client.get_job(&1);
        assert_eq!(job.budget_stroops, 1);
    }

    #[test]
    fn test_budget_at_maximum_succeeds() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &i128::MAX, &0, &0);
        let job = client.get_job(&1);
        assert_eq!(job.budget_stroops, i128::MAX);
    }

    #[test]
    #[should_panic(expected = "Contract, #5")]
    fn test_oversized_cid_panics_with_invalid_hash() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let long = Bytes::from_slice(&env, &[0u8; 65]);
        client.post_job(&1, &owner, &long, &1000, &0, &0);
    }

    #[test]
    #[should_panic(expected = "Contract, #5")]
    fn test_empty_hash_panics() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        client.post_job(&1, &owner, &hash(&env, b""), &1000, &0, &0);
    }

    // ── submit_bid ─────────────────────────────────────────────────────────
    #[test]
    fn test_submit_bid_success() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let bidder = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &bidder, &hash(&env, b"QmProposal"), &500);

        let bids = client.get_bids(&1);
        assert_eq!(bids.len(), 1);
        assert_eq!(bids.get(0).unwrap().freelancer, bidder);
    }

    #[test]
    #[should_panic(expected = "Contract, #7")]
    fn test_submit_bid_job_not_found() {
        let env = setup_env();
        let client = setup_client(&env);
        let bidder = Address::generate(&env);
        client.submit_bid(&999, &bidder, &hash(&env, b"QmProposal"), &500);
    }

    #[test]
    #[should_panic(expected = "Contract, #10")]
    fn test_duplicate_bid_panics() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let bidder = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &bidder, &hash(&env, b"QmProposal1"), &500);
        client.submit_bid(&1, &bidder, &hash(&env, b"QmProposal2"), &600);
    }

    #[test]
    fn test_multiple_bids_on_same_job() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let bidder1 = Address::generate(&env);
        let bidder2 = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &bidder1, &hash(&env, b"QmProp1"), &500);
        client.submit_bid(&1, &bidder2, &hash(&env, b"QmProp2"), &600);

        let bids = client.get_bids(&1);
        assert_eq!(bids.len(), 2);
    }

    #[test]
    fn test_multiple_jobs_and_bids() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let b1 = Address::generate(&env);
        let b2 = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmJ1"), &1000, &0, &0);
        client.post_job(&2, &owner, &hash(&env, b"QmJ2"), &2000, &0, &0);
        client.submit_bid(&1, &b1, &hash(&env, b"QmP1"), &500);
        client.submit_bid(&2, &b2, &hash(&env, b"QmP2"), &1500);

        assert_eq!(client.get_bids_count(&1), 1);
        assert_eq!(client.get_bids_count(&2), 1);
    }

    #[test]
    fn test_get_bids_count_empty_returns_zero() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        assert_eq!(client.get_bids_count(&1), 0);
    }

    #[test]
    fn test_get_bids_count_after_submissions() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let b1 = Address::generate(&env);
        let b2 = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &b1, &hash(&env, b"QmP1"), &500);
        client.submit_bid(&1, &b2, &hash(&env, b"QmP2"), &600);

        assert_eq!(client.get_bids_count(&1), 2);
    }

    #[test]
    #[should_panic(expected = "Contract, #8")]
    fn test_submit_bid_on_non_open_job() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let bidder = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        // First submit a bid for bidder so accept succeeds
        client.submit_bid(&1, &bidder, &hash(&env, b"QmProposal"), &500);
        client.accept_bid(&1, &owner, &bidder);
        // Should fail - job no longer Open
        client.submit_bid(&1, &bidder, &hash(&env, b"QmProposal2"), &600);
    }

    #[test]
    #[should_panic(expected = "Contract, #8")]
    fn test_bid_on_non_open_panics() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let bidder = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.close_job(&1, &owner);
        client.submit_bid(&1, &bidder, &hash(&env, b"QmProposal"), &500);
    }

    #[test]
    #[should_panic(expected = "Contract, #5")]
    fn test_submit_bid_empty_proposal_hash() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let bidder = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &bidder, &hash(&env, b""), &500);
    }

    #[test]
    #[should_panic(expected = "Contract, #13")]
    fn test_submit_bid_after_expiration_panics() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let bidder = Address::generate(&env);

        // bidding_deadline = 10
        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &10, &100);
        // Advance past deadline
        env.ledger().with_mut(|li| li.timestamp = 20);
        client.submit_bid(&1, &bidder, &hash(&env, b"QmProposal"), &500);
    }

    // ── cancel_bid ─────────────────────────────────────────────────────────
    #[test]
    fn test_cancel_bid_success() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let b1 = Address::generate(&env);
        let b2 = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &b1, &hash(&env, b"QmP1"), &500);
        client.submit_bid(&1, &b2, &hash(&env, b"QmP2"), &600);
        client.cancel_bid(&1, &b1);

        assert_eq!(client.get_bids_count(&1), 1);
        let remaining = client.get_bid_at(&1, &0);
        assert_eq!(remaining.freelancer, b2);
    }

    #[test]
    #[should_panic(expected = "Contract, #11")]
    fn test_cancel_nonexistent_bid() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let b1 = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.cancel_bid(&1, &b1);
    }

    // ── accept_bid ─────────────────────────────────────────────────────────
    #[test]
    fn test_accept_bid_success() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let b1 = Address::generate(&env);
        let b2 = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &b1, &hash(&env, b"QmP1"), &500);
        client.submit_bid(&1, &b2, &hash(&env, b"QmP2"), &600);
        client.accept_bid(&1, &owner, &b1);

        let job = client.get_job(&1);
        assert_eq!(job.status, JobStatus::Assigned);
        assert_eq!(job.freelancer, Some(b1.clone()));
    }

    #[test]
    fn test_full_lifecycle() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let freelancer = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmSomeIPFSHash"), &100000, &0, &0);
        client.submit_bid(&1, &freelancer, &hash(&env, b"QmProposalHash"), &1000);
        client.accept_bid(&1, &owner, &freelancer);

        let job = client.get_job(&1);
        assert_eq!(job.status, JobStatus::Assigned);
        assert_eq!(job.freelancer, Some(freelancer.clone()));

        client.submit_deliverable(&1, &freelancer, &hash(&env, b"QmDeliverableHash"));
        let job = client.get_job(&1);
        assert_eq!(job.status, JobStatus::DeliverableSubmitted);
    }

    #[test]
    #[should_panic(expected = "Contract, #8")]
    fn test_cannot_accept_bid_twice() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let b1 = Address::generate(&env);
        let b2 = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &b1, &hash(&env, b"QmP1"), &500);
        client.submit_bid(&1, &b2, &hash(&env, b"QmP2"), &600);
        client.accept_bid(&1, &owner, &b1);
        // Second attempt should fail
        client.accept_bid(&1, &owner, &b2);
    }

    #[test]
    #[should_panic(expected = "Contract, #9")]
    fn test_unauthorized_accept_bid() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let bidder = Address::generate(&env);
        let stranger = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &bidder, &hash(&env, b"QmProposal"), &500);
        client.accept_bid(&1, &stranger, &bidder);
    }

    #[test]
    #[should_panic(expected = "Contract, #11")]
    fn test_accept_bid_requires_existing_bid() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let fake = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.accept_bid(&1, &owner, &fake);
    }

    #[test]
    #[should_panic(expected = "Contract, #13")]
    fn test_accept_bid_after_expiration_panics() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let bidder = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &10, &100);
        client.submit_bid(&1, &bidder, &hash(&env, b"QmProposal"), &500);
        env.ledger().with_mut(|li| li.timestamp = 20);
        client.accept_bid(&1, &owner, &bidder);
    }

    #[test]
    fn test_get_bids_page_first_window() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let b1 = Address::generate(&env);
        let b2 = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &b1, &hash(&env, b"QmP1"), &500);
        client.submit_bid(&1, &b2, &hash(&env, b"QmP2"), &600);

        let page = client.get_bids_page(&1, &0, &1);
        assert_eq!(page.len(), 1);
        assert_eq!(page.get(0).unwrap().freelancer, b1);
    }

    #[test]
    fn test_get_bids_page_second_window() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let b1 = Address::generate(&env);
        let b2 = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &b1, &hash(&env, b"QmP1"), &500);
        client.submit_bid(&1, &b2, &hash(&env, b"QmP2"), &600);

        let page = client.get_bids_page(&1, &1, &1);
        assert_eq!(page.len(), 1);
        assert_eq!(page.get(0).unwrap().freelancer, b2);
    }

    #[test]
    fn test_get_bids_page_offset_beyond_end_returns_empty() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let b1 = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &b1, &hash(&env, b"QmP1"), &500);

        let page = client.get_bids_page(&1, &5, &10);
        assert_eq!(page.len(), 0);
    }

    // ── deliverable & dispute ──────────────────────────────────────────────
    #[test]
    fn test_submit_deliverable_success() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let f = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &f, &hash(&env, b"QmP"), &500);
        client.accept_bid(&1, &owner, &f);
        client.submit_deliverable(&1, &f, &hash(&env, b"QmDeliverable"));

        let job = client.get_job(&1);
        assert_eq!(job.status, JobStatus::DeliverableSubmitted);
    }

    #[test]
    #[should_panic(expected = "Contract, #9")]
    fn test_submit_deliverable_unauthorized() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let f = Address::generate(&env);
        let imposter = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &f, &hash(&env, b"QmP"), &500);
        client.accept_bid(&1, &owner, &f);
        client.submit_deliverable(&1, &imposter, &hash(&env, b"QmBad"));
    }

    #[test]
    fn test_get_deliverable_without_submission_returns_assigned() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let f = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &f, &hash(&env, b"QmP"), &500);
        client.accept_bid(&1, &owner, &f);
        // submit_deliverable was not called - job stays Assigned
        let job = client.get_job(&1);
        assert_eq!(job.status, JobStatus::Assigned);
    }

    #[test]
    fn test_mark_disputed_from_assigned() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let f = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &f, &hash(&env, b"QmP"), &500);
        client.accept_bid(&1, &owner, &f);
        client.mark_disputed(&1, &owner);

        let job = client.get_job(&1);
        assert_eq!(job.status, JobStatus::Disputed);
    }

    #[test]
    fn test_mark_disputed_from_deliverable_submitted() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let f = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &f, &hash(&env, b"QmP"), &500);
        client.accept_bid(&1, &owner, &f);
        client.submit_deliverable(&1, &f, &hash(&env, b"QmDeliverable"));
        client.mark_disputed(&1, &f);

        let job = client.get_job(&1);
        assert_eq!(job.status, JobStatus::Disputed);
    }

    #[test]
    #[should_panic(expected = "Contract, #12")]
    fn test_mark_disputed_from_open_fails() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.mark_disputed(&1, &owner);
    }

    #[test]
    #[should_panic(expected = "Contract, #12")]
    fn test_mark_disputed_from_open_panics() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.mark_disputed(&1, &owner);
    }

    // ── close / cancel expiry ─────────────────────────────────────────────
    #[test]
    fn test_close_job() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.close_job(&1, &owner);

        let job = client.get_job(&1);
        assert_eq!(job.status, JobStatus::Closed);
    }

    #[test]
    fn test_cancel_expired_job_by_client() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);

        // expires_at = 100
        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &100);
        env.ledger().with_mut(|li| li.timestamp = 200);
        client.cancel_expired_job(&1, &owner);

        let job = client.get_job(&1);
        assert_eq!(job.status, JobStatus::Closed);
    }

    #[test]
    #[should_panic(expected = "Contract, #12")]
    fn test_cancel_expired_job_before_expiration_panics() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);

        // expires_at far in the future relative to current ledger time
        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &1_700_000_100);
        // current time = 1_700_000_000 < 1_700_000_100, not expired yet
        client.cancel_expired_job(&1, &owner);
    }

    // ── get_job / get_bid edge cases ──────────────────────────────────────
    #[test]
    #[should_panic(expected = "Contract, #7")]
    fn test_get_job_not_found() {
        let env = setup_env();
        let client = setup_client(&env);
        client.get_job(&999);
    }

    #[test]
    #[should_panic(expected = "Contract, #15")]
    fn test_get_bid_at_out_of_bounds() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.get_bid_at(&1, &0);
    }

    #[test]
    fn test_multiple_jobs_isolated() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner_1 = Address::generate(&env);
        let owner_2 = Address::generate(&env);
        let bidder_1 = Address::generate(&env);
        let bidder_2 = Address::generate(&env);

        client.post_job(&1, &owner_1, &hash(&env, b"QmJ1"), &1000, &0, &0);
        client.post_job(&2, &owner_2, &hash(&env, b"QmJ2"), &2000, &0, &0);
        client.submit_bid(&1, &bidder_1, &hash(&env, b"QmP1"), &800);
        client.submit_bid(&2, &bidder_2, &hash(&env, b"QmP2"), &1700);
        client.accept_bid(&1, &owner_1, &bidder_1);

        let job_1 = client.get_job(&1);
        let job_2 = client.get_job(&2);

        assert_eq!(job_1.status, JobStatus::Assigned);
        assert_eq!(job_1.freelancer, Some(bidder_1));
        assert_eq!(job_2.status, JobStatus::Open);
        assert_eq!(job_2.freelancer, None);
        assert_eq!(client.get_bids_count(&1), 1);
        assert_eq!(client.get_bids_count(&2), 1);
    }

    // ── upgrade admin ──────────────────────────────────────────────────────
    #[test]
    fn test_upgrade_admin_initialize_and_read() {
        let env = setup_env();
        let contract_id = env.register_contract(None, JobRegistryContract);
        let client = JobRegistryContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let new_admin = Address::generate(&env);
        client.set_upgrade_admin(&admin, &new_admin);
        let stored = client.get_upgrade_admin();
        assert_eq!(stored, Some(new_admin));
    }

    #[test]
    #[should_panic(expected = "Contract, #9")]
    fn test_set_upgrade_admin_requires_current_admin() {
        let env = setup_env();
        let client = setup_client(&env);
        let admin = Address::generate(&env);
        let stranger = Address::generate(&env);
        let new_admin = Address::generate(&env);

        client.set_upgrade_admin(&stranger, &new_admin);
    }

    // ── budget edge cases ──────────────────────────────────────────────────
    #[test]
    fn test_budget_above_maximum_succeeds() {
        // No maximum enforcement; only minimum > 0
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &i128::MAX, &0, &0);
        let job = client.get_job(&1);
        assert_eq!(job.budget_stroops, i128::MAX);
    }

    // ── late bid after acceptance ──────────────────────────────────────────
    #[test]
    #[should_panic(expected = "Contract, #8")]
    fn test_late_bid_after_acceptance_panics_with_job_not_open() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let bidder = Address::generate(&env);
        let late = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &bidder, &hash(&env, b"QmP1"), &500);
        client.accept_bid(&1, &owner, &bidder);
        // Late bidder tries to bid on an already-assigned job
        client.submit_bid(&1, &late, &hash(&env, b"QmLate"), &600);
    }

    // ── accept without matching bid ────────────────────────────────────────
    #[test]
    #[should_panic(expected = "Contract, #11")]
    fn test_accept_without_matching_bid_panics() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let b1 = Address::generate(&env);
        let nobody = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.submit_bid(&1, &b1, &hash(&env, b"QmP1"), &500);
        // Try accepting a bidder that never submitted
        client.accept_bid(&1, &owner, &nobody);
    }

    // ── accept_bid on empty bid list ───────────────────────────────────────
    #[test]
    #[should_panic(expected = "Contract, #11")]
    fn test_accept_bid_with_no_bids_submitted() {
        let env = setup_env();
        let client = setup_client(&env);
        let owner = Address::generate(&env);
        let nobody = Address::generate(&env);

        client.post_job(&1, &owner, &hash(&env, b"QmHash"), &1000, &0, &0);
        client.accept_bid(&1, &owner, &nobody);
    }

    // ── get_bids on missing job ────────────────────────────────────────────
    #[test]
    #[should_panic(expected = "Contract, #7")]
    fn test_get_bids_job_not_found() {
        let env = setup_env();
        let client = setup_client(&env);
        client.get_bids(&999);
    }

    #[test]
    #[should_panic(expected = "Contract, #7")]
    fn test_get_bids_for_missing_job_panics() {
        let env = setup_env();
        let client = setup_client(&env);
        client.get_bids(&999);
    }

    // ── set_escrow_deployer (alias for upgrade_admin round-trip) ──────────
    // In this contract the "escrow deployer" role is not separate; the
    // upgrade admin serves as the upgrade authority.
    #[test]
    fn test_set_escrow_deployer_round_trip() {
        let env = setup_env();
        let contract_id = env.register_contract(None, JobRegistryContract);
        let client = JobRegistryContractClient::new(&env, &contract_id);
        let admin = Address::generate(&env);
        client.initialize(&admin);

        let deployer = Address::generate(&env);
        client.set_upgrade_admin(&admin, &deployer);
        let stored = client.get_upgrade_admin();
        assert_eq!(stored, Some(deployer));
    }
}
