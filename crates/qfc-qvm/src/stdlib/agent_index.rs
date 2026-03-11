//! Agent Discovery Index
//!
//! In-memory index for fast agent discovery by capability, protocol digest, and reputation.
//! Rebuilt from state on node startup and incrementally updated on each block.

use std::collections::{BTreeSet, HashMap, HashSet};

use primitive_types::H160;
use qfc_types::Hash;

use super::agent_registry::{AgentEvent, AgentRegistration};

/// View of an agent for index queries (lightweight copy of key fields)
#[derive(Clone, Debug)]
pub struct AgentView {
    pub id: Hash,
    pub owner: H160,
    pub capabilities: Vec<String>,
    pub protocol_digests: Vec<Hash>,
    pub endpoint: String,
    pub stake: u64,
    pub frozen: bool,
    pub reputation_score: u64,
    pub total_tasks_completed: u64,
    pub last_heartbeat: u64,
    pub created_at: u64,
}

impl AgentView {
    pub fn from_registration(reg: &AgentRegistration) -> Self {
        Self {
            id: reg.id,
            owner: reg.owner,
            capabilities: reg.capabilities.clone(),
            protocol_digests: reg.protocol_digests.clone(),
            endpoint: reg.endpoint.clone(),
            stake: reg.stake,
            frozen: reg.frozen,
            reputation_score: reg.reputation_score,
            total_tasks_completed: reg.total_tasks_completed,
            last_heartbeat: reg.last_heartbeat,
            created_at: reg.created_at,
        }
    }
}

/// Sort key for BTreeSet: (reputation descending via Reverse, agent_id for uniqueness)
/// We store (Reverse(reputation), agent_id) so BTreeSet natural order gives highest reputation first.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ReputationKey(std::cmp::Reverse<u64>, Hash);

/// In-memory index for fast agent discovery
pub struct AgentIndex {
    /// capability -> sorted set of (reputation_desc, agent_id)
    by_capability: HashMap<String, BTreeSet<ReputationKey>>,
    /// protocol_digest -> set of agent_ids
    by_protocol: HashMap<Hash, HashSet<Hash>>,
    /// All agents by ID
    agents: HashMap<Hash, AgentView>,
}

impl AgentIndex {
    pub fn new() -> Self {
        Self {
            by_capability: HashMap::new(),
            by_protocol: HashMap::new(),
            agents: HashMap::new(),
        }
    }

    /// Rebuild from a collection of agent registrations
    pub fn rebuild<'a>(&mut self, agents: impl Iterator<Item = &'a AgentRegistration>) {
        self.by_capability.clear();
        self.by_protocol.clear();
        self.agents.clear();

        for reg in agents {
            self.insert_agent(AgentView::from_registration(reg));
        }
    }

    /// Insert or update an agent in the index
    pub fn insert_agent(&mut self, view: AgentView) {
        let id = view.id;
        let rep_key = ReputationKey(std::cmp::Reverse(view.reputation_score), id);

        // Remove old entries if updating
        if let Some(old) = self.agents.remove(&id) {
            let old_key = ReputationKey(std::cmp::Reverse(old.reputation_score), id);
            for cap in &old.capabilities {
                if let Some(set) = self.by_capability.get_mut(cap) {
                    set.remove(&old_key);
                }
            }
            for digest in &old.protocol_digests {
                if let Some(set) = self.by_protocol.get_mut(digest) {
                    set.remove(&id);
                }
            }
        }

        // Insert new entries
        for cap in &view.capabilities {
            self.by_capability
                .entry(cap.clone())
                .or_default()
                .insert(rep_key.clone());
        }
        for digest in &view.protocol_digests {
            self.by_protocol.entry(*digest).or_default().insert(id);
        }

        self.agents.insert(id, view);
    }

    /// Remove an agent from the index
    pub fn remove_agent(&mut self, agent_id: &Hash) {
        if let Some(old) = self.agents.remove(agent_id) {
            let old_key = ReputationKey(std::cmp::Reverse(old.reputation_score), *agent_id);
            for cap in &old.capabilities {
                if let Some(set) = self.by_capability.get_mut(cap) {
                    set.remove(&old_key);
                }
            }
            for digest in &old.protocol_digests {
                if let Some(set) = self.by_protocol.get_mut(digest) {
                    set.remove(agent_id);
                }
            }
        }
    }

    /// Get agent view by ID
    pub fn get(&self, agent_id: &Hash) -> Option<&AgentView> {
        self.agents.get(agent_id)
    }

    /// Query agents by capability with filters, sorted by reputation (descending)
    pub fn query_by_capability(
        &self,
        capability: &str,
        min_reputation: u64,
        min_stake: u64,
        limit: usize,
    ) -> Vec<&AgentView> {
        let Some(set) = self.by_capability.get(capability) else {
            return Vec::new();
        };

        set.iter()
            .filter_map(|key| {
                let agent = self.agents.get(&key.1)?;
                if agent.frozen {
                    return None;
                }
                if agent.reputation_score < min_reputation {
                    return None;
                }
                if agent.stake < min_stake {
                    return None;
                }
                Some(agent)
            })
            .take(limit.max(1))
            .collect()
    }

    /// Query agents by protocol digest
    pub fn query_by_protocol_digest(&self, digest: &Hash) -> Vec<&AgentView> {
        let Some(set) = self.by_protocol.get(digest) else {
            return Vec::new();
        };

        set.iter()
            .filter_map(|id| {
                let agent = self.agents.get(id)?;
                if agent.frozen {
                    return None;
                }
                Some(agent)
            })
            .collect()
    }

    /// List all agents with optional filters
    pub fn list_agents(
        &self,
        status: AgentStatusFilter,
        sort_by: AgentSortField,
        sort_desc: bool,
        limit: usize,
        offset: usize,
    ) -> (Vec<&AgentView>, usize) {
        let mut agents: Vec<&AgentView> = self
            .agents
            .values()
            .filter(|a| match status {
                AgentStatusFilter::Active => !a.frozen,
                AgentStatusFilter::Frozen => a.frozen,
                AgentStatusFilter::All => true,
            })
            .collect();

        let total = agents.len();

        match sort_by {
            AgentSortField::ReputationScore => {
                agents.sort_by_key(|a| a.reputation_score);
            }
            AgentSortField::Stake => {
                agents.sort_by_key(|a| a.stake);
            }
            AgentSortField::CreatedAt => {
                agents.sort_by_key(|a| a.created_at);
            }
        }

        if sort_desc {
            agents.reverse();
        }

        let result = agents
            .into_iter()
            .skip(offset)
            .take(limit.max(1).min(200))
            .collect();

        (result, total)
    }

    /// Apply events from a block to incrementally update the index
    pub fn apply_events(
        &mut self,
        events: &[AgentEvent],
        get_agent: impl Fn(&Hash) -> Option<AgentView>,
    ) {
        for event in events {
            match event {
                AgentEvent::Registered { agent_id, .. }
                | AgentEvent::Updated { agent_id, .. }
                | AgentEvent::Frozen { agent_id, .. }
                | AgentEvent::Unfrozen { agent_id, .. }
                | AgentEvent::Slashed { agent_id, .. }
                | AgentEvent::HeartbeatUpdated { agent_id, .. } => {
                    if let Some(view) = get_agent(agent_id) {
                        self.insert_agent(view);
                    }
                }
                AgentEvent::Revoked { agent_id, .. } => {
                    self.remove_agent(agent_id);
                }
            }
        }
    }

    /// Number of indexed agents
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    /// Number of distinct capabilities indexed
    pub fn capability_count(&self) -> usize {
        self.by_capability.len()
    }
}

impl Default for AgentIndex {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Filter/Sort Enums ───

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatusFilter {
    Active,
    Frozen,
    All,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSortField {
    ReputationScore,
    Stake,
    CreatedAt,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_hash(seed: &[u8]) -> Hash {
        let mut h = [0u8; 32];
        let b = blake3::hash(seed);
        h.copy_from_slice(b.as_bytes());
        Hash::from(h)
    }

    fn owner(n: u8) -> H160 {
        H160::from([n; 20])
    }

    fn make_view(seed: &[u8], caps: Vec<&str>, rep: u64, stake: u64) -> AgentView {
        AgentView {
            id: test_hash(seed),
            owner: owner(1),
            capabilities: caps.into_iter().map(String::from).collect(),
            protocol_digests: vec![],
            endpoint: "https://x.com".to_string(),
            stake,
            frozen: false,
            reputation_score: rep,
            total_tasks_completed: 0,
            last_heartbeat: 0,
            created_at: 1000,
        }
    }

    #[test]
    fn test_insert_and_get() {
        let mut idx = AgentIndex::new();
        let view = make_view(b"a1", vec!["text-gen"], 5000, 1000);
        let id = view.id;
        idx.insert_agent(view);
        assert!(idx.get(&id).is_some());
        assert_eq!(idx.agent_count(), 1);
    }

    #[test]
    fn test_remove_agent() {
        let mut idx = AgentIndex::new();
        let view = make_view(b"r1", vec!["text-gen"], 5000, 1000);
        let id = view.id;
        idx.insert_agent(view);
        idx.remove_agent(&id);
        assert!(idx.get(&id).is_none());
        assert_eq!(idx.agent_count(), 0);
    }

    #[test]
    fn test_query_by_capability() {
        let mut idx = AgentIndex::new();
        idx.insert_agent(make_view(b"c1", vec!["text-gen"], 8000, 1000));
        idx.insert_agent(make_view(b"c2", vec!["text-gen"], 6000, 500));
        idx.insert_agent(make_view(b"c3", vec!["image-analysis"], 9000, 2000));

        let results = idx.query_by_capability("text-gen", 0, 0, 10);
        assert_eq!(results.len(), 2);
        // Should be sorted by reputation descending
        assert!(results[0].reputation_score >= results[1].reputation_score);
    }

    #[test]
    fn test_query_by_capability_min_reputation() {
        let mut idx = AgentIndex::new();
        idx.insert_agent(make_view(b"r1", vec!["text-gen"], 8000, 1000));
        idx.insert_agent(make_view(b"r2", vec!["text-gen"], 3000, 1000));

        let results = idx.query_by_capability("text-gen", 5000, 0, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].reputation_score, 8000);
    }

    #[test]
    fn test_query_by_capability_min_stake() {
        let mut idx = AgentIndex::new();
        idx.insert_agent(make_view(b"s1", vec!["text-gen"], 5000, 10_000));
        idx.insert_agent(make_view(b"s2", vec!["text-gen"], 5000, 100));

        let results = idx.query_by_capability("text-gen", 0, 5000, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].stake, 10_000);
    }

    #[test]
    fn test_query_by_capability_limit() {
        let mut idx = AgentIndex::new();
        for i in 0..10u8 {
            idx.insert_agent(make_view(
                &[i],
                vec!["text-gen"],
                5000 + i as u64 * 100,
                1000,
            ));
        }

        let results = idx.query_by_capability("text-gen", 0, 0, 3);
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn test_query_excludes_frozen() {
        let mut idx = AgentIndex::new();
        let mut view = make_view(b"frz", vec!["text-gen"], 9000, 1000);
        view.frozen = true;
        idx.insert_agent(view);

        let results = idx.query_by_capability("text-gen", 0, 0, 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_query_nonexistent_capability() {
        let idx = AgentIndex::new();
        let results = idx.query_by_capability("nonexistent", 0, 0, 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_query_by_protocol_digest() {
        let mut idx = AgentIndex::new();
        let digest = test_hash(b"proto1");
        let mut view = make_view(b"p1", vec!["text-gen"], 5000, 1000);
        view.protocol_digests = vec![digest];
        idx.insert_agent(view);

        let results = idx.query_by_protocol_digest(&digest);
        assert_eq!(results.len(), 1);

        let results = idx.query_by_protocol_digest(&test_hash(b"other"));
        assert!(results.is_empty());
    }

    #[test]
    fn test_list_agents_active_filter() {
        let mut idx = AgentIndex::new();
        idx.insert_agent(make_view(b"a1", vec![], 5000, 1000));
        let mut frozen = make_view(b"a2", vec![], 3000, 1000);
        frozen.frozen = true;
        idx.insert_agent(frozen);

        let (active, _) = idx.list_agents(
            AgentStatusFilter::Active,
            AgentSortField::ReputationScore,
            true,
            10,
            0,
        );
        assert_eq!(active.len(), 1);

        let (frozen_list, _) = idx.list_agents(
            AgentStatusFilter::Frozen,
            AgentSortField::ReputationScore,
            true,
            10,
            0,
        );
        assert_eq!(frozen_list.len(), 1);

        let (all, total) = idx.list_agents(
            AgentStatusFilter::All,
            AgentSortField::ReputationScore,
            true,
            10,
            0,
        );
        assert_eq!(all.len(), 2);
        assert_eq!(total, 2);
    }

    #[test]
    fn test_list_agents_sort_by_reputation() {
        let mut idx = AgentIndex::new();
        idx.insert_agent(make_view(b"s1", vec![], 3000, 1000));
        idx.insert_agent(make_view(b"s2", vec![], 9000, 1000));
        idx.insert_agent(make_view(b"s3", vec![], 6000, 1000));

        let (agents, _) = idx.list_agents(
            AgentStatusFilter::All,
            AgentSortField::ReputationScore,
            true,
            10,
            0,
        );
        assert_eq!(agents[0].reputation_score, 9000);
        assert_eq!(agents[1].reputation_score, 6000);
        assert_eq!(agents[2].reputation_score, 3000);
    }

    #[test]
    fn test_list_agents_pagination() {
        let mut idx = AgentIndex::new();
        for i in 0..5u8 {
            let mut v = make_view(&[i], vec![], 5000, 1000);
            v.created_at = i as u64 * 100;
            idx.insert_agent(v);
        }

        let (page1, total) = idx.list_agents(
            AgentStatusFilter::All,
            AgentSortField::CreatedAt,
            true,
            2,
            0,
        );
        assert_eq!(page1.len(), 2);
        assert_eq!(total, 5);

        let (page2, _) = idx.list_agents(
            AgentStatusFilter::All,
            AgentSortField::CreatedAt,
            true,
            2,
            2,
        );
        assert_eq!(page2.len(), 2);
    }

    #[test]
    fn test_update_agent_reindexes() {
        let mut idx = AgentIndex::new();
        let view = make_view(b"upd", vec!["text-gen"], 5000, 1000);
        let id = view.id;
        idx.insert_agent(view);

        // Update with new capabilities
        let mut updated = make_view(b"upd", vec!["image-analysis"], 8000, 2000);
        updated.id = id; // same ID
        idx.insert_agent(updated);

        // Should no longer appear in text-gen
        let results = idx.query_by_capability("text-gen", 0, 0, 10);
        assert!(results.is_empty());

        // Should appear in image-analysis
        let results = idx.query_by_capability("image-analysis", 0, 0, 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].reputation_score, 8000);
    }

    #[test]
    fn test_apply_events() {
        let mut idx = AgentIndex::new();
        let id = test_hash(b"ev-agent");
        let view = make_view(b"ev-agent", vec!["text-gen"], 5000, 1000);

        // Simulate registration event
        let events = vec![AgentEvent::Registered {
            agent_id: id,
            owner: owner(1),
            capabilities: vec!["text-gen".to_string()],
            stake: 1000,
        }];

        idx.apply_events(&events, |_| Some(view.clone()));
        assert_eq!(idx.agent_count(), 1);

        // Simulate revocation event
        let events = vec![AgentEvent::Revoked {
            agent_id: id,
            stake_returned: 1000,
            stake_slashed: 0,
        }];
        idx.apply_events(&events, |_| None);
        assert_eq!(idx.agent_count(), 0);
    }

    #[test]
    fn test_rebuild() {
        let mut idx = AgentIndex::new();
        // Pre-populate with something
        idx.insert_agent(make_view(b"old", vec!["x"], 1000, 100));

        let regs = vec![
            AgentRegistration {
                id: test_hash(b"r1"),
                owner: owner(1),
                protocol_digests: vec![],
                capabilities: vec!["text-gen".to_string()],
                endpoint: "https://a.com".to_string(),
                stake: 5000,
                frozen: false,
                reputation_score: 7000,
                total_tasks_completed: 10,
                total_tasks_failed: 0,
                last_heartbeat: 1000,
                created_at: 500,
                revoke_initiated_at: 0,
            },
            AgentRegistration {
                id: test_hash(b"r2"),
                owner: owner(2),
                protocol_digests: vec![test_hash(b"proto")],
                capabilities: vec!["image-analysis".to_string()],
                endpoint: "https://b.com".to_string(),
                stake: 10000,
                frozen: false,
                reputation_score: 9000,
                total_tasks_completed: 50,
                total_tasks_failed: 1,
                last_heartbeat: 2000,
                created_at: 600,
                revoke_initiated_at: 0,
            },
        ];

        idx.rebuild(regs.iter());
        // Old agent should be gone, new ones present
        assert_eq!(idx.agent_count(), 2);
        assert!(idx.get(&test_hash(b"old")).is_none());
        assert!(idx.get(&test_hash(b"r1")).is_some());
    }
}
