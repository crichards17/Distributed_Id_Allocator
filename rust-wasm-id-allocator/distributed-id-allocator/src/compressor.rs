pub(crate) mod persistence;
pub(crate) mod persistence_utils;
pub(crate) mod tables;
use self::persistence::DeserializationError;
use self::tables::final_space::FinalSpace;
use self::tables::session_space::{ClusterRef, SessionSpace, SessionSpaceRef, Sessions};
use self::tables::session_space_normalizer::SessionSpaceNormalizer;
use id_types::*;

/// The reserved value for an unknown token index.
/// Used in interop.
pub const NIL_TOKEN: i64 = -1;

#[derive(Debug)]
/// The core allocator.
pub struct IdCompressor {
    session_id: SessionId,
    local_session: SessionSpaceRef,
    generated_id_count: u64,
    next_range_base_generation_count: u64,
    sessions: Sessions,
    final_space: FinalSpace,
    // Cache of the last finalized final ID in final space. Used to optimize normalization.
    final_id_limit: FinalId,
    session_space_normalizer: SessionSpaceNormalizer,
    cluster_capacity: u64,
    telemetry_stats: TelemetryStats,
}

impl IdCompressor {
    /// Returns the current default for cluster sizing.
    pub fn get_default_cluster_capacity() -> u64 {
        persistence::DEFAULT_CLUSTER_CAPACITY
    }

    #[cfg(feature = "uuid-generation")]
    /// Instantiates a new allocator with a random session ID.
    /// Only available when the "uuid-generation" feature is enabled.
    pub fn new() -> Self {
        let session_id = SessionId::new();
        IdCompressor::new_with_session_id(session_id)
    }

    /// Instantiates a new allocator with the supplied SessionId.
    pub fn new_with_session_id(session_id: SessionId) -> Self {
        let mut sessions = Sessions::new();
        IdCompressor {
            session_id,
            local_session: sessions.get_or_create(session_id),
            generated_id_count: 0,
            next_range_base_generation_count: LocalId::from_id(-1).to_generation_count(),
            sessions,
            final_space: FinalSpace::new(),
            final_id_limit: FinalId::from_id(0),
            session_space_normalizer: SessionSpaceNormalizer::new(),
            cluster_capacity: persistence::DEFAULT_CLUSTER_CAPACITY,
            telemetry_stats: TelemetryStats::EMPTY,
        }
    }

    /// Returns this compressor's session ID.
    pub fn get_local_session_id(&self) -> SessionId {
        self.session_id
    }

    /// Returns a reference to this compressor's session space.
    fn get_local_session_space(&self) -> &SessionSpace {
        self.sessions.deref_session_space(self.local_session)
    }

    /// Returns a token representing the supplied session ID, or an error if no such session has been seen by the compressor.
    /// The returned token (if any) is valid for the lifetime of the compressor and is usable in place of a SessionId in APIs that accept it.
    /// Performance note: calling APIs with a token results in better performance than using a [SessionId], so repeated calls will benefit from
    /// first converting the [SessionId] to a token.
    ///
    /// > # Errors
    /// > * `AllocatorError::NoTokenForSession`
    /// >   * No known session for the provided SessionId.
    pub fn get_session_token_from_session_id(
        &self,
        session_id: SessionId,
    ) -> Result<i64, AllocatorError> {
        match self.sessions.get(session_id) {
            None => Err(AllocatorError::NoTokenForSession),
            Some(session_space) => Ok(session_space.self_ref().get_index() as i64),
        }
    }

    /// Returns the current sizing used for new clusters.
    pub fn get_cluster_capacity(&self) -> u64 {
        self.cluster_capacity
    }

    /// Updates the sizing used for new cluster creation.
    ///
    /// > # Errors
    /// > * `AllocatorError::InvalidClusterCapacity`
    /// >   * The supplied cluster size must be a non-zero integer.
    ///
    pub fn set_cluster_capacity(
        &mut self,
        new_cluster_capacity: u64,
    ) -> Result<(), AllocatorError> {
        if new_cluster_capacity < 1 {
            Err(AllocatorError::InvalidClusterCapacity)
        } else {
            self.cluster_capacity = new_cluster_capacity;
            Ok(())
        }
    }

    /// Generates and returns this compressor's next session space ID.
    pub fn generate_next_id(&mut self) -> SessionSpaceId {
        self.generated_id_count += 1;
        let tail_cluster = match self.get_local_session_space().get_tail_cluster() {
            Some(tail_cluster_ref) => self.sessions.deref_cluster(tail_cluster_ref),
            None => {
                // No cluster, return next local
                return self.generate_next_local_id().into();
            }
        };
        let cluster_offset =
            self.generated_id_count - tail_cluster.base_local_id.to_generation_count();
        if tail_cluster.capacity > cluster_offset {
            // Space in the cluster: eager final
            self.telemetry_stats.eager_final_count += 1;
            (tail_cluster.base_final_id + cluster_offset).into()
        } else {
            // No space in the cluster, return next local
            self.generate_next_local_id().into()
        }
    }

    fn generate_next_local_id(&mut self) -> LocalId {
        self.telemetry_stats.local_id_count += 1;
        let new_local = LocalId::from_id(-(self.generated_id_count as i64));
        self.session_space_normalizer.add_local_range(new_local, 1);
        new_local
    }

    /// Returns current compressor state telemetry.
    /// Intended for logging and analysis.
    pub fn get_telemetry_stats(&mut self) -> TelemetryStats {
        let stats = self.telemetry_stats;
        self.telemetry_stats = TelemetryStats::EMPTY;
        stats
    }

    /// Returns a range of IDs (if any) created by this session since the last range generation.
    /// If no IDs have been created since the last range generation, the range field of the [IdRange] will be None.
    pub fn take_next_range(&mut self) -> IdRange {
        let count = self.generated_id_count - (self.next_range_base_generation_count - 1);
        IdRange {
            id: self.session_id,
            range: if count == 0 {
                None
            } else {
                assert!(
                    count > 0,
                    "Must only allocate a positive number of IDs. Count was {}",
                    count
                );
                let next_range = Some((self.next_range_base_generation_count, count));
                self.next_range_base_generation_count = self.generated_id_count + 1;
                next_range
            },
        }
    }

    /// Finalizes the supplied range of IDs (which may be from either a remote or local session).
    /// This method encapsulates the total order broadcast logic which guarantees state synchronization between multiple networked compressors.
    /// Operation acknowledgement must call this method.
    pub fn finalize_range(
        &mut self,
        &IdRange {
            id: session_id,
            range,
        }: &IdRange,
    ) -> Result<(), AllocatorError> {
        // Check if the block has IDs
        let (range_base_gen_count, range_len) = match range {
            None => {
                return Ok(());
            }
            Some((_, 0)) => {
                return Err(AllocatorError::MalformedIdRange);
            }
            Some(range) => range,
        };

        let range_base_local = LocalId::from_generation_count(range_base_gen_count);
        let range_base_stable = StableId::from(session_id) + range_base_local;
        // Checks collision for the maximum new-cluster span (the condition in which the current tail cluster is exactly full)
        if self.sessions.range_collides(
            session_id,
            range_base_stable,
            range_base_stable + range_len + self.cluster_capacity,
        ) {
            return Err(AllocatorError::ClusterCollision);
        }
        let session_space_ref = self.sessions.get_or_create(session_id);
        let tail_cluster_ref = match self
            .sessions
            .deref_session_space_mut(session_space_ref)
            .get_tail_cluster()
        {
            Some(tail_cluster) => tail_cluster,
            None => {
                // This is the first cluster in the session
                if range_base_local != -1 {
                    return Err(AllocatorError::RangeFinalizedOutOfOrder);
                }
                self.telemetry_stats.cluster_creation_count += 1;
                self.add_empty_cluster(
                    session_space_ref,
                    range_base_local,
                    self.cluster_capacity + range_len,
                )
            }
        };
        let tail_cluster = self.sessions.deref_cluster_mut(tail_cluster_ref);
        let remaining_capacity = tail_cluster.capacity - tail_cluster.count;
        if tail_cluster.base_local_id - tail_cluster.count != range_base_local {
            return Err(AllocatorError::RangeFinalizedOutOfOrder);
        }
        if remaining_capacity >= range_len {
            // The current IdBlock range fits in the existing cluster
            tail_cluster.count += range_len;
        } else {
            let overflow = range_len - remaining_capacity;
            let new_claimed_final_count = overflow + self.cluster_capacity;
            if self.final_space.is_last(tail_cluster_ref) {
                // Tail_cluster is the last cluster, and so can be expanded.
                self.telemetry_stats.expansion_count += 1;
                tail_cluster.capacity += new_claimed_final_count;
                tail_cluster.count += range_len;
            } else {
                // Tail_cluster is not the last cluster. Fill and overflow to new.
                self.telemetry_stats.cluster_creation_count += 1;
                tail_cluster.count = tail_cluster.capacity;
                let new_cluster_ref = self.add_empty_cluster(
                    session_space_ref,
                    range_base_local - remaining_capacity,
                    new_claimed_final_count,
                );
                self.sessions.deref_cluster_mut(new_cluster_ref).count += overflow;
            }
        }
        self.final_id_limit = match self.final_space.get_tail_cluster(&self.sessions) {
            Some(cluster) => cluster.base_final_id + cluster.count,
            None => self.final_id_limit,
        };
        Ok(())
    }

    fn add_empty_cluster(
        &mut self,
        session_space_ref: SessionSpaceRef,
        base_local: LocalId,
        capacity: u64,
    ) -> ClusterRef {
        let next_base_final = match self.final_space.get_tail_cluster(&self.sessions) {
            Some(cluster) => cluster.base_final_id + cluster.capacity,
            None => FinalId::from_id(0),
        };
        let session_space = self.sessions.deref_session_space_mut(session_space_ref);
        let new_cluster_ref =
            session_space.add_empty_cluster(next_base_final, base_local, capacity);
        self.final_space
            .add_cluster(new_cluster_ref, &self.sessions);

        new_cluster_ref
    }

    /// Normalizes a session space ID to op space.
    /// Returns the [OpSpaceId] equivalent for the provided [SessionSpaceId], if applicable.
    ///
    /// > # Errors
    /// > * `AllocatorError::InvalidSessionSpaceId`
    /// >   * The provided [SessionSpaceId] has not been allocated.
    pub fn normalize_to_op_space(&self, id: SessionSpaceId) -> Result<OpSpaceId, AllocatorError> {
        match id.to_space() {
            CompressedId::Final(final_id) => Ok(OpSpaceId::from(final_id)),
            CompressedId::Local(local_id) => {
                if !self.session_space_normalizer.contains(local_id) {
                    Err(AllocatorError::InvalidSessionSpaceId)
                } else {
                    let local_session_space = self.get_local_session_space();
                    match local_session_space.try_convert_to_final(local_id, true) {
                        Some(converted_final) => Ok(OpSpaceId::from(converted_final)),
                        None => Ok(OpSpaceId::from(local_id)),
                    }
                }
            }
        }
    }

    /// Normalizes an op space ID to this session's session space.
    /// Requires the ID originator's session ID as a SessionId.
    /// Returns the [SessionSpaceId] equivalent for the provided [OpSpaceId], if applicable.
    ///
    /// > # Errors
    /// > * `AllocatorError::NoTokenForSession`
    /// >   * No known session for the provided [SessionId].
    /// > * `AllocatorError::InvalidOpSpaceId`
    /// >   * Failed to normalize the provided [OpSpaceId].
    pub fn normalize_to_session_space(
        &self,
        id: OpSpaceId,
        originator: SessionId,
    ) -> Result<SessionSpaceId, AllocatorError> {
        let token = match self.get_session_token_from_session_id(originator) {
            Ok(token) => token,
            Err(err) => {
                if id.is_local() {
                    return Err(err);
                } else {
                    NIL_TOKEN
                }
            }
        };
        self.normalize_to_session_space_with_token(id, token)
    }

    /// Normalizes an op space ID to this session's session space.
    /// Requires the ID originator's session token.
    /// Returns the [SessionSpaceId] equivalent for the provided [OpSpaceId], if applicable.
    ///
    /// > # Errors
    /// > * `AllocatorError::InvalidOpSpaceId`
    /// >   * Failed to normalize the provided [OpSpaceId].
    pub fn normalize_to_session_space_with_token(
        &self,
        id: OpSpaceId,
        originator_token: i64,
    ) -> Result<SessionSpaceId, AllocatorError> {
        match id.to_space() {
            CompressedId::Local(local_to_normalize) => {
                let originator_ref = SessionSpaceRef::create_from_token(originator_token);
                if originator_ref == self.local_session {
                    if self.session_space_normalizer.contains(local_to_normalize) {
                        Ok(SessionSpaceId::from(local_to_normalize))
                    } else if local_to_normalize.to_generation_count() <= self.generated_id_count {
                        // Id is an eager final

                        match self
                            .get_local_session_space()
                            .try_convert_to_final(local_to_normalize, true)
                        {
                            None => return Err(AllocatorError::InvalidOpSpaceId),
                            Some(allocated_final) => Ok(allocated_final.into()),
                        }
                    } else {
                        return Err(AllocatorError::InvalidOpSpaceId);
                    }
                } else {
                    // LocalId from a foreign session
                    let foreign_session_space = self.sessions.deref_session_space(originator_ref);
                    match foreign_session_space.try_convert_to_final(local_to_normalize, false) {
                        Some(final_id) => Ok(SessionSpaceId::from(final_id)),
                        None => Err(AllocatorError::InvalidOpSpaceId),
                    }
                }
            }
            CompressedId::Final(final_to_normalize) => {
                match self
                    .get_local_session_space()
                    .get_cluster_by_allocated_final(final_to_normalize)
                {
                    // Exists in local cluster chain
                    Some(containing_cluster) => {
                        let aligned_local =
                            match containing_cluster.get_aligned_local(final_to_normalize) {
                                None => return Err(AllocatorError::InvalidOpSpaceId),
                                Some(aligned_local) => aligned_local,
                            };
                        if self.session_space_normalizer.contains(aligned_local) {
                            Ok(SessionSpaceId::from(aligned_local))
                        } else if aligned_local.to_generation_count() <= self.generated_id_count {
                            Ok(SessionSpaceId::from(final_to_normalize))
                        } else {
                            Err(AllocatorError::InvalidOpSpaceId)
                        }
                    }
                    None => {
                        // Does not exist in local cluster chain
                        if final_to_normalize >= self.final_id_limit {
                            Err(AllocatorError::InvalidOpSpaceId)
                        } else {
                            Ok(SessionSpaceId::from(final_to_normalize))
                        }
                    }
                }
            }
        }
    }

    /// Decompresses a session space ID to its stable ID equivalent.
    /// Can decompress finalized IDs, as well as allocated local-session IDs.
    /// Returns the [StableId] equivalent of the passed [SessionSpaceId], if able.
    ///
    /// > # Errors
    /// > * `AllocatorError::InvalidSessionSpaceId`
    /// >   * Failed to decompress the provided [SessionSpaceId].
    pub fn decompress(&self, id: SessionSpaceId) -> Result<StableId, AllocatorError> {
        match id.to_space() {
            CompressedId::Final(final_id) => {
                match self.final_space.search(final_id, &self.sessions) {
                    Some(containing_cluster) => {
                        let aligned_local = match containing_cluster.get_aligned_local(final_id) {
                            None => return Err(AllocatorError::InvalidSessionSpaceId),
                            Some(aligned_local) => aligned_local,
                        };
                        if aligned_local < containing_cluster.max_local() {
                            // must be an id generated (allocated or finalized) by the local session, or a finalized id from a remote session
                            if containing_cluster.session_creator == self.local_session {
                                if self.session_space_normalizer.contains(aligned_local) {
                                    return Err(AllocatorError::InvalidSessionSpaceId);
                                }
                                if aligned_local.to_generation_count() > self.generated_id_count {
                                    return Err(AllocatorError::InvalidSessionSpaceId);
                                }
                            } else {
                                return Err(AllocatorError::InvalidSessionSpaceId);
                            }
                        }

                        Ok(self
                            .sessions
                            .deref_session_space(containing_cluster.session_creator)
                            .session_id()
                            + aligned_local)
                    }
                    None => Err(AllocatorError::InvalidSessionSpaceId),
                }
            }
            CompressedId::Local(local_id) => {
                if !self.session_space_normalizer.contains(local_id) {
                    return Err(AllocatorError::InvalidSessionSpaceId);
                }
                Ok(self.session_id + local_id)
            }
        }
    }

    /// Recompresses a stable ID to its session space ID equivalent.
    /// Returns the `SessionSpaceId` equivalent for the given `StableId`, if able.
    ///
    /// > # Errors
    /// > * `AllocatorError::InvalidStableId`
    /// >   * Failed to recompress the provided `StableId`.
    pub fn recompress(&self, id: StableId) -> Result<SessionSpaceId, AllocatorError> {
        match self.sessions.get_containing_cluster(id) {
            None => {
                let session_as_stable = StableId::from(self.session_id);
                if id >= session_as_stable {
                    let gen_count_equivalent = id - session_as_stable + 1;
                    if gen_count_equivalent <= self.generated_id_count as u128 {
                        // Is a locally generated ID, with or without a finalized cluster
                        let local_equivalent =
                            LocalId::from_generation_count(gen_count_equivalent as u64);
                        if self.session_space_normalizer.contains(local_equivalent) {
                            return Ok(SessionSpaceId::from(local_equivalent));
                        }
                    }
                }
                Err(AllocatorError::InvalidStableId)
            }
            Some((cluster, originator_local)) => {
                if cluster.session_creator == self.local_session {
                    // Local session
                    if self.session_space_normalizer.contains(originator_local) {
                        Ok(SessionSpaceId::from(originator_local))
                    } else if originator_local.to_generation_count() <= self.generated_id_count {
                        // Id is an eager final
                        match cluster.get_allocated_final(originator_local) {
                            None => return Err(AllocatorError::InvalidStableId),
                            Some(allocated_final) => Ok(allocated_final.into()),
                        }
                    } else {
                        return Err(AllocatorError::InvalidStableId);
                    }
                } else {
                    //Not the local session
                    if originator_local.to_generation_count()
                        < cluster.base_local_id.to_generation_count() + cluster.count
                    {
                        match cluster.get_allocated_final(originator_local) {
                            None => Err(AllocatorError::InvalidStableId),
                            Some(allocated_final) => Ok(allocated_final.into()),
                        }
                    } else {
                        Err(AllocatorError::InvalidStableId)
                    }
                }
            }
        }
    }

    /// Returns a persistable form of the current state of this `IdCompressor`, either with or without local state.
    /// Serializing without local state includes only finalized state, and is therefore suitable for use in summaries.
    /// Serializing with local state includes finalized state as well as un-finalized state and is therefore suitable for use in offline scenarios.
    /// Either form can be rehydrated via `IdCompressor::deserialize()`.
    pub fn serialize(&self, include_local_state: bool) -> Vec<u8> {
        if !include_local_state {
            persistence::v1::serialize(self)
        } else {
            persistence::v1::serialize_with_local(self)
        }
    }

    #[cfg(feature = "uuid-generation")]
    /// Rehydrates a serialized `IdCompressor`, providing a random [SessionId] if rehydrating without local state.
    /// Enabled by the `uuid-generation` feature.
    pub fn deserialize(bytes: &[u8]) -> Result<IdCompressor, DeserializationError> {
        persistence::deserialize(bytes, SessionId::new)
    }

    /// Rehydrates a serialized `IdCompressor`.
    /// The provided `FMakeSession` function must be able to return a session ID in order to rehydrate without local state.
    pub fn deserialize_with_session_id_generator<FMakeSession>(
        bytes: &[u8],
        make_session_id: FMakeSession,
    ) -> Result<IdCompressor, DeserializationError>
    where
        FMakeSession: FnOnce() -> SessionId,
    {
        persistence::deserialize(bytes, make_session_id)
    }
}

#[cfg(debug_assertions)]
impl IdCompressor {
    /// Checks equality across [IdCompressor]_s.
    /// Debug-only, intended for testing.
    pub fn equals_test_only(&self, other: &IdCompressor, compare_local_state: bool) -> bool {
        if !(self.final_id_limit == other.final_id_limit
            && self.sessions.equals_test_only(&other.sessions)
            && self.final_space.equals_test_only(
                &other.final_space,
                &self.sessions,
                &other.sessions,
            )
            && self.cluster_capacity == other.cluster_capacity)
        {
            false
        } else {
            !(compare_local_state
                && !(self.session_id == other.session_id
                    && self.generated_id_count == other.generated_id_count
                    && self.next_range_base_generation_count
                        == other.next_range_base_generation_count
                    && self.session_space_normalizer == other.session_space_normalizer))
        }
    }
}

#[derive(Debug)]
/// A struct for communicating ID range data.
pub struct IdRange {
    /// The originating-session identifier.
    pub id: SessionId,
    /// A Some(range) will contain a tuple of u64s representing `(First ID, count of IDs)`.
    pub range: Option<(u64, u64)>,
}

#[derive(Debug, Copy, Clone)]
/// A struct for containing relevant telemetry values for direct logging or interop transmission.
/// Intended for internal use.
pub struct TelemetryStats {
    /// Count of allocated eager finals.
    pub eager_final_count: u64,
    /// Count of allocated local IDs.
    pub local_id_count: u64,
    /// Count of instances of tail cluster expansion.
    pub expansion_count: u64,
    /// Count of new clusters created.
    pub cluster_creation_count: u64,
}

impl TelemetryStats {
    const EMPTY: TelemetryStats = TelemetryStats {
        eager_final_count: 0,
        local_id_count: 0,
        expansion_count: 0,
        cluster_creation_count: 0,
    };
}
