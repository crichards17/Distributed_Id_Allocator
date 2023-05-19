use avl::AvlTreeMap;
use id_types::local_id::local_id_from_id;
use id_types::session_id::{session_id_from_id_u128, session_id_from_stable_id};
use id_types::{FinalId, LocalId, SessionId, StableId};
use std::cmp::Ordering;
use std::mem::size_of;
use std::ops::Bound;

/// The local/UUID space within an individual Session.
/// Effectively represents the cluster chain for a given session.
pub struct Sessions {
    session_map: AvlTreeMap<SessionId, SessionSpaceRef>,
    session_list: Vec<SessionSpace>,
    // Session IDs are stored in little endian byte form in a compact array to accelerate serialization.
    // When a session ID is queried (via session space ref) the bytes are converted to a u128/Session ID.
    // This tradeoff favors serialization at a small penalty to the (already slow) path of decompress/recompress.
    // NOTE: this is indexed by session_space_ref.index * sizeof(u128)
    session_ids: Vec<u8>,
}

impl Sessions {
    pub fn new() -> Sessions {
        Sessions {
            session_map: AvlTreeMap::new(),
            session_list: Vec::new(),
            session_ids: Vec::new(),
        }
    }

    pub fn get_session_count(&self) -> usize {
        self.session_list.len()
    }

    pub fn get_or_create(&mut self, session_id: SessionId) -> SessionSpaceRef {
        *self.session_map.entry(session_id).or_insert_with(|| {
            let new_session_space_index = self.session_list.len();
            let new_session_space_ref = SessionSpaceRef {
                index: new_session_space_index,
            };
            let new_session_space = SessionSpace::new();
            self.session_list.push(new_session_space);
            self.session_ids
                .extend_from_slice(&<[u8; 16]>::from(session_id));
            new_session_space_ref
        })
    }

    pub fn get_session_id(&self, session_space_ref: SessionSpaceRef) -> SessionId {
        let index = session_space_ref.index * size_of::<u128>();
        let bytes: [u8; 16] = self.session_ids[index..index + size_of::<u128>()]
            .try_into()
            .unwrap();
        session_id_from_id_u128(u128::from_le_bytes(bytes))
    }

    pub fn get(&self, session_id: SessionId) -> Option<&SessionSpaceRef> {
        self.session_map.get(&session_id)
    }

    pub fn deref_session_space_mut(
        &mut self,
        session_space_ref: SessionSpaceRef,
    ) -> &mut SessionSpace {
        &mut self.session_list[session_space_ref.index]
    }

    pub fn deref_session_space(&self, session_space_ref: SessionSpaceRef) -> &SessionSpace {
        debug_assert!(
            session_space_ref.index < self.session_list.len(),
            "Out of bounds session space ref."
        );
        &self.session_list[session_space_ref.index]
    }

    pub fn deref_cluster_mut(&mut self, cluster_ref: ClusterRef) -> &mut IdCluster {
        &mut self
            .deref_session_space_mut(cluster_ref.session_space_ref)
            .cluster_chain[cluster_ref.cluster_chain_index]
    }

    pub fn deref_cluster(&self, cluster_ref: ClusterRef) -> &IdCluster {
        &self
            .deref_session_space(cluster_ref.session_space_ref)
            .cluster_chain[cluster_ref.cluster_chain_index]
    }

    pub fn get_session_id_slice(&self) -> &[u8] {
        &self.session_ids
    }

    pub fn get_containing_cluster(
        &self,
        query: StableId,
    ) -> Option<(&IdCluster, SessionSpaceRef, LocalId)> {
        let mut range = self
            .session_map
            .range((
                Bound::Excluded(SessionId::nil()),
                Bound::Included(session_id_from_stable_id(query)),
            ))
            .rev();
        match range.next() {
            None => None,
            Some((_, &session_space_ref)) => {
                let session_space = self.deref_session_space(session_space_ref);
                let session_id = self.get_session_id(session_space_ref);
                if query > session_space.get_max_allocated_stable(session_id) {
                    return None;
                }
                let delta: u128 = query - StableId::from(session_id);
                let aligned_local = LocalId::from_generation_count(delta as u64 + 1);
                match session_space.get_cluster_by_local(aligned_local, true) {
                    Some(cluster_match) => {
                        let result_session_id = session_id;
                        let cluster_min_stable = result_session_id + cluster_match.base_local_id;
                        let cluster_max_stable = cluster_min_stable + cluster_match.capacity;
                        if query >= cluster_min_stable && query <= cluster_max_stable {
                            let originator_local = local_id_from_id(
                                -((query - StableId::from(result_session_id)) as i64) - 1,
                            );
                            Some((cluster_match, session_space_ref, originator_local))
                        } else {
                            None
                        }
                    }
                    None => None,
                }
            }
        }
    }

    pub fn range_collides(
        &self,
        originator: SessionId,
        range_base: StableId,
        range_max: StableId,
    ) -> bool {
        let mut range = self
            .session_map
            .range((
                Bound::Excluded(SessionId::nil()),
                Bound::Included(session_id_from_stable_id(range_max)),
            ))
            .rev();
        match range.next() {
            None => false,
            Some((_, &session_space_ref)) => {
                let session_space = self.deref_session_space(session_space_ref);
                let session_id = self.get_session_id(session_space_ref);
                originator != session_id
                    && range_base <= session_space.get_max_allocated_stable(session_id)
            }
        }
    }

    #[cfg(debug_assertions)]
    pub(crate) fn equals_test_only(&self, other: &Sessions) -> bool {
        fn get_sorted_sessions(
            sessions: &Sessions,
        ) -> impl Iterator<Item = (&SessionSpace, SessionId)> {
            let mut filtered: Vec<(&SessionSpace, SessionId)> = (0..sessions.session_list.len())
                .filter(|index| !sessions.session_list[*index].cluster_chain_is_empty())
                .map(|index| {
                    (
                        &sessions.session_list[index],
                        sessions.get_session_id(SessionSpaceRef { index }),
                    )
                })
                .collect();
            filtered
                .sort_by(|&(_, session_id_a), &(_, session_id_b)| session_id_a.cmp(&session_id_b));
            filtered.into_iter()
        }
        let mut filtered_a = get_sorted_sessions(self);
        let mut filtered_b = get_sorted_sessions(other);
        loop {
            let session_space_a = filtered_a.next();
            let session_space_b = filtered_b.next();
            if session_space_a.is_none() != session_space_b.is_none() {
                return false;
            }
            if session_space_a.is_none() && session_space_b.is_none() {
                return true;
            }
            let (session_space_a, session_id_a) = session_space_a.unwrap();
            let (session_space_b, session_id_b) = session_space_b.unwrap();
            if session_id_a != session_id_b
                || session_space_a.cluster_chain != session_space_b.cluster_chain
            {
                return false;
            }
            if !self.session_map.contains_key(&session_id_a)
                || !other.session_map.contains_key(&session_id_b)
            {
                return false;
            }
        }
    }

    fn session_id_vec(&self) -> Vec<String> {
        let mut session_ids: Vec<String> = Vec::new();
        for index in (0..self.session_ids.len()).step_by(16) {
            let bytes: [u8; 16] = self.session_ids[index..index + size_of::<u128>()]
                .try_into()
                .unwrap();
            session_ids.push(session_id_from_id_u128(u128::from_le_bytes(bytes)).into());
        }
        session_ids
    }
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#?}", self.session_map)?;
        write!(f, "{:#?}", self.session_list)?;
        write!(f, " session_ids (UUID form): ")?;
        write!(f, "{:#?}", &self.session_id_vec())
    }
}

#[derive(Debug)]
pub struct SessionSpace {
    // All clusters in the session space, sorted on LocalId.
    cluster_chain: Vec<IdCluster>,
}

impl SessionSpace {
    pub fn new() -> SessionSpace {
        SessionSpace {
            cluster_chain: Vec::new(),
        }
    }

    pub fn cluster_chain_is_empty(&self) -> bool {
        self.cluster_chain.is_empty()
    }

    pub fn get_tail_cluster(&self) -> Option<&IdCluster> {
        if self.cluster_chain.is_empty() {
            return None;
        }
        Some(&self.cluster_chain[self.cluster_chain.len() - 1])
    }

    pub fn get_tail_cluster_mut(&mut self) -> Option<&mut IdCluster> {
        if self.cluster_chain.is_empty() {
            return None;
        }
        let last = self.cluster_chain.len() - 1;
        Some(&mut self.cluster_chain[last])
    }

    fn get_max_allocated_stable(&self, session_id: SessionId) -> StableId {
        let tail_cluster = match self.cluster_chain.len() {
            0 => return session_id.into(),
            len => &self.cluster_chain[len - 1],
        };
        session_id + tail_cluster.max_allocated_local()
    }

    pub fn add_empty_cluster(
        &mut self,
        self_ref: SessionSpaceRef,
        base_final_id: FinalId,
        base_local_id: LocalId,
        capacity: u64,
    ) -> ClusterRef {
        let new_cluster = IdCluster {
            base_final_id,
            base_local_id,
            capacity,
            count: 0,
        };
        self.add_cluster(self_ref, new_cluster)
    }

    pub fn add_cluster(&mut self, self_ref: SessionSpaceRef, new_cluster: IdCluster) -> ClusterRef {
        self.cluster_chain.push(new_cluster);
        let tail_index = self.cluster_chain.len() - 1;
        ClusterRef {
            session_space_ref: self_ref,
            cluster_chain_index: tail_index,
        }
    }

    pub fn try_convert_to_final(
        &self,
        search_local: LocalId,
        include_allocated: bool,
    ) -> Option<FinalId> {
        self.get_cluster_by_local(search_local, include_allocated)
            .map(|found_cluster| found_cluster.get_allocated_final(search_local).unwrap())
    }

    fn get_cluster_by_local(
        &self,
        search_local: LocalId,
        include_allocated: bool,
    ) -> Option<&IdCluster> {
        let last_valid_local: fn(current_cluster: &IdCluster) -> u64 = if include_allocated {
            |current_cluster| current_cluster.capacity - 1
        } else {
            |current_cluster| current_cluster.count - 1
        };

        match self.cluster_chain.binary_search_by(|current_cluster| {
            let cluster_last_local =
                current_cluster.base_local_id - last_valid_local(current_cluster);
            if cluster_last_local > search_local {
                Ordering::Less
            } else if current_cluster.base_local_id < search_local {
                return Ordering::Greater;
            } else {
                Ordering::Equal
            }
        }) {
            Ok(index) => Some(&self.cluster_chain[index]),
            Err(_) => None,
        }
    }

    // TODO: include contract about allocated not finalized
    pub fn get_cluster_by_allocated_final(&self, search_final: FinalId) -> Option<&IdCluster> {
        match self.cluster_chain.binary_search_by(|current_cluster| {
            let cluster_base_final = current_cluster.base_final_id;
            let cluster_last_final = cluster_base_final + (current_cluster.capacity - 1);
            if cluster_last_final < search_final {
                Ordering::Less
            } else if cluster_base_final > search_final {
                return Ordering::Greater;
            } else {
                Ordering::Equal
            }
        }) {
            Ok(found_cluster_index) => Some(&self.cluster_chain[found_cluster_index]),
            Err(_) => None,
        }
    }
}

#[derive(Debug)]
pub struct IdCluster {
    pub(crate) base_final_id: FinalId,
    pub(crate) base_local_id: LocalId,
    pub(crate) capacity: u64,
    pub(crate) count: u64,
}

impl IdCluster {
    pub fn get_allocated_final(&self, local_within: LocalId) -> Option<FinalId> {
        let cluster_offset =
            local_within.to_generation_count() - self.base_local_id.to_generation_count();
        if cluster_offset < self.capacity {
            Some(self.base_final_id + cluster_offset)
        } else {
            None
        }
    }

    pub fn get_aligned_local(&self, contained_final: FinalId) -> Option<LocalId> {
        if contained_final < self.base_final_id || contained_final > self.max_allocated_final() {
            return None;
        }
        let final_delta = contained_final - self.base_final_id;
        Some(self.base_local_id - final_delta as u64)
    }

    pub fn max_allocated_final(&self) -> FinalId {
        self.base_final_id + (self.capacity - 1)
    }

    pub fn max_local(&self) -> LocalId {
        self.base_local_id - (self.count - 1)
    }

    pub fn max_allocated_local(&self) -> LocalId {
        self.base_local_id - (self.capacity - 1)
    }
}

impl PartialEq for IdCluster {
    fn eq(&self, other: &Self) -> bool {
        self.base_final_id == other.base_final_id
            && self.base_local_id == other.base_local_id
            && self.capacity == other.capacity
            && self.count == other.count
    }
}

// Maps to an index in the session_list
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SessionSpaceRef {
    index: usize,
}

impl SessionSpaceRef {
    pub fn get_index(&self) -> usize {
        self.index
    }

    pub fn create_from_token(token: i64) -> SessionSpaceRef {
        debug_assert!(token >= 0, "Nil token passed as session space ref.");
        SessionSpaceRef {
            index: token as usize,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClusterRef {
    session_space_ref: SessionSpaceRef,
    cluster_chain_index: usize,
}

impl ClusterRef {
    pub fn get_session_space_ref(&self) -> SessionSpaceRef {
        self.session_space_ref
    }

    #[cfg(debug_assertions)]
    pub(crate) fn equals_test_only(
        &self,
        other: &ClusterRef,
        sessions_self: &Sessions,
        sessions_other: &Sessions,
    ) -> bool {
        let session_id_a = sessions_self.get_session_id(self.session_space_ref);
        let session_id_b = sessions_other.get_session_id(other.session_space_ref);
        session_id_a == session_id_b && self.cluster_chain_index == other.cluster_chain_index
    }
}
