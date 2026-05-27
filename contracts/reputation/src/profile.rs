use soroban_sdk::{contracttype, Address, Bytes, Env};

/// Badge tiers keyed in the metadata map.
#[contracttype]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BadgeTier {
    Bronze,
    Silver,
    Gold,
    Platinum,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct BadgeMetadataEntry {
    pub tier: BadgeTier,
    /// IPFS CID (or any URI) pointing to the badge image / JSON metadata.
    pub uri: Bytes,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub address: Address,
    pub client_score: i32,
    pub client_points: i32,
    pub client_jobs: u32,
    pub freelancer_score: i32,
    pub freelancer_points: i32,
    pub freelancer_jobs: u32,
    pub metadata_hash: Option<Bytes>,
    /// Per-tier badge metadata URIs set by the admin.
    pub badge_metadata: soroban_sdk::Vec<BadgeMetadataEntry>,
}

impl Profile {
    pub fn new(_env: &Env, address: Address) -> Self {
        Self {
            address,
            client_score: 5000,
            client_points: 0,
            client_jobs: 0,
            freelancer_score: 5000,
            freelancer_points: 0,
            freelancer_jobs: 0,
            metadata_hash: None,
            badge_metadata: soroban_sdk::Vec::new(_env),
        }
    }

    pub fn default(_env: Env) -> Self {
        // This is tricky because we need an address.
        // We'll leave it to the caller to provide an address.
        panic!("Profile needs an address; use new(env, address)")
    }
}
