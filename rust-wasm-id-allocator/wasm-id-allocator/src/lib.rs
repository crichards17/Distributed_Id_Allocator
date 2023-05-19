#![deny(
    bad_style,
    dead_code,
    improper_ctypes,
    non_shorthand_field_patterns,
    no_mangle_generic_items,
    overflowing_literals,
    path_statements,
    patterns_in_fns_without_body,
    private_in_public,
    unconditional_recursion,
    unused,
    unused_allocation,
    unused_comparisons,
    unused_parens,
    while_true,
    missing_docs,
    trivial_casts,
    trivial_numeric_casts,
    unused_extern_crates,
    unused_import_braces,
    unused_qualifications,
    unused_results
)]

//! This crate contains logic and structs for adapting the distributed-id-allocator crate to WASM.
//! It adjusts data representations and usage patterns to a form suitable for interop between JavaScript and WASM.
//! For example, numeric types are converted to and from F64, as that type can natively pass across the interop boundary without additional marshalling logic.
//! Additionally, APIs used in hot paths have their error logic queried separately to avoid passing complex data types back and forth during the common use case.

use distributed_id_allocator::compressor::{IdCompressor as IdCompressorCore, IdRange, NIL_TOKEN};
use id_types::{
    errors::ErrorString, AllocatorError, OpSpaceId, SessionId, SessionSpaceId, StableId,
};
use std::f64::NAN;
use wasm_bindgen::prelude::*;

#[cfg(target_arch = "wasm32")]
use lol_alloc::{AssumeSingleThreaded, FreeListAllocator};

// SAFETY: This application is single threaded, so using AssumeSingleThreaded is allowed.
#[cfg(target_arch = "wasm32")]
#[global_allocator]
static ALLOCATOR: AssumeSingleThreaded<FreeListAllocator> =
    unsafe { AssumeSingleThreaded::new(FreeListAllocator::new()) };

// macro_rules! log {
//     ( $( $t:tt )* ) => {
//         #[cfg(test)]
//         web_sys::console::log_1(&format!( $( $t )* ).into());
//     }
// }

#[wasm_bindgen]
#[derive(Debug)]
/// A wrapper compressor for efficient API translation from/into WASM.
pub struct IdCompressor {
    compressor: IdCompressorCore,
}

const MAX_DEFAULT_CLUSTER_CAPACITY: f64 = 2_i32.pow(11) as f64;

#[wasm_bindgen]
impl IdCompressor {
    /// Returns the default cluster capacity. This static is exposed on the compressor to comply with wasm-bindgen.
    pub fn get_default_cluster_capacity() -> f64 {
        IdCompressorCore::get_default_cluster_capacity() as f64
    }

    /// Returns the compressor's NIL_TOKEN static value as an f64.
    pub fn get_nil_token() -> f64 {
        NIL_TOKEN as f64
    }

    #[wasm_bindgen(constructor)]
    /// Creates a new compressor with the supplied session ID.
    /// The ID string should be a valid v4 UUID string.
    pub fn new(session_id_string: String) -> Result<IdCompressor, JsError> {
        Ok(IdCompressor {
            compressor: IdCompressorCore::new_with_session_id(
                SessionId::from_uuid_string(&session_id_string).map_err(into_jserror)?,
            ),
        })
    }

    /// Returns the local session ID UUID.
    /// For interop performance, this method returns a byte array containing the ASCII representation of the UUID string.
    /// Should not be called frequently due to marshalling costs.
    pub fn get_local_session_id(&self) -> Vec<u8> {
        Vec::from(StableId::from(self.compressor.get_local_session_id()))
    }

    /// Returns the current cluster capacity.
    /// See [distributed_id_allocator::compressor::IdCompressor] for more.
    pub fn get_cluster_capacity(&self) -> f64 {
        self.compressor.get_cluster_capacity() as f64
    }

    /// Sets the current cluster capacity.
    /// See [distributed_id_allocator::compressor::IdCompressor] for more.
    pub fn set_cluster_capacity(&mut self, new_cluster_capacity: f64) -> Result<(), JsError> {
        if new_cluster_capacity.fract() != 0.0 || new_cluster_capacity < 0.0 {
            return Err(into_jserror(AllocatorError::InvalidClusterCapacity));
        }
        if new_cluster_capacity > MAX_DEFAULT_CLUSTER_CAPACITY {
            return Err(JsError::new("Clusters must not exceed max cluster size."));
        }
        self.compressor
            .set_cluster_capacity(new_cluster_capacity as u64)
            .map_err(into_jserror)
    }

    /// Generates a new ID.
    /// See [distributed_id_allocator::compressor::IdCompressor] for more.
    pub fn generate_next_id(&mut self) -> f64 {
        self.compressor.generate_next_id().id() as f64
    }

    /// Returns a number token associated with the supplied session UUID string.
    /// Returns a NIL token if the session ID has never been associated with a finalization by this compressor.
    /// Throws an error if the UUID string is not well formed.
    /// This API exists as an optimization to avoid repeatedly passing strings across the interop boundary and allows a user
    /// to instead cheaply pass a number representing that string. The result can be cached to avoid future calls to this method.
    pub fn get_token(&mut self, session_uuid_string: String) -> Result<f64, JsError> {
        Ok(self
            .compressor
            .get_session_token_from_session_id(
                SessionId::from_uuid_string(&session_uuid_string).map_err(into_jserror)?,
            )
            .map(|x| x as f64)
            .unwrap_or(IdCompressor::get_nil_token()))
    }

    /// Returns a range of IDs (if any) created by this session.
    /// See [distributed_id_allocator::compressor::IdCompressor] for more.
    pub fn take_next_range(&mut self) -> Option<InteropIds> {
        self.compressor
            .take_next_range()
            .range
            .map(|(first_local_gen_count, count)| InteropIds {
                first_local_gen_count: first_local_gen_count as f64,
                count: count as f64,
            })
    }

    /// Finalizes a range of IDs.
    /// See [distributed_id_allocator::compressor::IdCompressor] for more.
    pub fn finalize_range(
        &mut self,
        session_id_str: String,
        range_base_count: f64,
        range_len: f64,
    ) -> Result<Option<InteropTelemetryStats>, JsError> {
        self.compressor
            .finalize_range(&IdRange {
                id: SessionId::from_uuid_string(&session_id_str).map_err(into_jserror)?,
                range: Some((range_base_count as u64, range_len as u64)),
            })
            .map_err(into_jserror)?;
        let stats = self.compressor.get_telemetry_stats();
        Ok(Some(InteropTelemetryStats {
            eager_final_count: stats.eager_final_count as f64,
            local_id_count: stats.local_id_count as f64,
            expansion_count: stats.expansion_count as f64,
            cluster_creation_count: stats.cluster_creation_count as f64,
        }))
    }

    /// Normalizes the ID from session space to op space.
    /// For performance reasons, NAN will be returned in the event of an error and the corresponding error
    /// string can be retrieved by calling `get_normalization_error_string`.
    /// See [distributed_id_allocator::compressor::IdCompressor] for more.
    pub fn normalize_to_op_space(&mut self, session_space_id: f64) -> f64 {
        match &self
            .compressor
            .normalize_to_op_space(SessionSpaceId::from_id(session_space_id as i64))
        {
            Err(_) => NAN,
            Ok(op_space_id) => op_space_id.id() as f64,
        }
    }

    /// Normalizes the ID from op space to session space given the token representing the session ID for the originating session.
    /// For performance reasons, NAN will be returned in the event of an error and the corresponding error
    /// string can be retrieved by calling `get_normalization_error_string`.
    /// See [distributed_id_allocator::compressor::IdCompressor] for more.
    pub fn normalize_to_session_space(&mut self, op_space_id: f64, originator_token: f64) -> f64 {
        // Safe to cast token because the TS layer sends nil token iff passing FinalId and a SessionId it has not tokenized.
        // This can occur when normalizing an ID referenced by a client that has not finalized any IDs,
        // and thus is not yet in the Sessions list.
        match &self.compressor.normalize_to_session_space_with_token(
            OpSpaceId::from_id(op_space_id as i64),
            originator_token as i64,
        ) {
            Err(_) => NAN,
            Ok(session_space_id) => session_space_id.id() as f64,
        }
    }

    /// Decompresses the ID into the corresponding UUID string.
    /// For interop performance, this method returns a byte array containing the ASCII representation of the UUID string.
    /// See [distributed_id_allocator::compressor::IdCompressor] for more.
    pub fn decompress(&mut self, id_to_decompress: f64) -> Option<Vec<u8>> {
        let stable_id = self
            .compressor
            .decompress(SessionSpaceId::from_id(id_to_decompress as i64))
            .ok()?;
        Some(stable_id.into())
    }

    /// Recompresses the UUID string into the corresponding ID.
    /// See [distributed_id_allocator::compressor::IdCompressor] for more.
    pub fn recompress(&mut self, id_to_recompress: String) -> Option<f64> {
        Some(
            self.compressor
                .recompress(StableId::from(
                    SessionId::from_uuid_string(&id_to_recompress).ok()?,
                ))
                .ok()?
                .id() as f64,
        )
    }

    /// Returns the serialized compressor.
    /// See [distributed_id_allocator::compressor::IdCompressor] for more.
    pub fn serialize(&self, include_local_state: bool) -> Vec<u8> {
        self.compressor.serialize(include_local_state)
    }

    /// Returns the deserialized compressor.
    /// See [distributed_id_allocator::compressor::IdCompressor] for more.
    pub fn deserialize(bytes: &[u8], session_id_string: String) -> Result<IdCompressor, JsError> {
        let session_id = SessionId::from_uuid_string(&session_id_string).map_err(into_jserror)?;
        Ok(IdCompressor {
            compressor: IdCompressorCore::deserialize_with_session_id_generator(bytes, || {
                session_id
            })
            .map_err(into_jserror)?,
        })
    }

    #[cfg(feature = "debug-format")]
    /// Returns the the pretty-print debug representation of the compressor state.
    /// Debug-only.
    pub fn to_debug_string(&self) -> String {
        format!("{:#?}", self.compressor)
    }
}

#[wasm_bindgen]
/// Struct for passing telemetry information across the interop boundary.
pub struct InteropTelemetryStats {
    eager_final_count: f64,
    local_id_count: f64,
    expansion_count: f64,
    cluster_creation_count: f64,
}

#[wasm_bindgen]
impl InteropTelemetryStats {
    /// See [distributed_id_allocator::compressor::TelemetryStats] for more.
    #[wasm_bindgen(getter)]
    pub fn eager_final_count(&self) -> f64 {
        self.eager_final_count
    }

    /// See [distributed_id_allocator::compressor::TelemetryStats] for more.
    #[wasm_bindgen(getter)]
    pub fn local_id_count(&self) -> f64 {
        self.local_id_count
    }

    /// See [distributed_id_allocator::compressor::TelemetryStats] for more.
    #[wasm_bindgen(getter)]
    pub fn expansion_count(&self) -> f64 {
        self.expansion_count
    }

    /// See [distributed_id_allocator::compressor::TelemetryStats] for more.
    #[wasm_bindgen(getter)]
    pub fn cluster_creation_count(&self) -> f64 {
        self.cluster_creation_count
    }
}

#[wasm_bindgen]
#[derive(Clone, Copy)]
/// Struct for passing ID ranges across the interop boundary.
pub struct InteropIds {
    first_local_gen_count: f64,
    count: f64,
}

#[wasm_bindgen]
impl InteropIds {
    /// See [distributed_id_allocator::compressor::TelemetryStats] for more.
    #[wasm_bindgen(getter)]
    pub fn first_local_gen_count(&self) -> f64 {
        self.first_local_gen_count
    }

    /// See [distributed_id_allocator::compressor::TelemetryStats] for more.
    #[wasm_bindgen(getter)]
    pub fn count(&self) -> f64 {
        self.count
    }
}

fn into_jserror(error: impl ErrorString) -> JsError {
    JsError::new(error.to_error_string())
}

#[wasm_bindgen]
#[cfg(debug_assertions)]
/// Increments the supplied UUID and returns the result.
/// Debug-only.
pub fn increment_uuid(_uuid_string: String, _offset: f64) -> Result<Vec<u8>, JsError> {
    Ok(
        (StableId::from(SessionId::from_uuid_string(&_uuid_string).ok().unwrap())
            + (_offset as u64))
            .into(),
    )
}

#[wasm_bindgen]
#[cfg(debug_assertions)]
/// Compares the two supplied compressors for equality (factoring in local state or not depending on the flag).
/// Debug-only.
pub fn compressor_equals(
    _a: &IdCompressor,
    _b: &IdCompressor,
    _compare_local_state: bool,
) -> Result<bool, JsError> {
    #[cfg(debug_assertions)]
    Ok(_a
        .compressor
        .equals_test_only(&_b.compressor, _compare_local_state))
}

#[cfg(test)]
mod tests {
    use super::*;
    use id_types::{local_id::get_id_from_local_id, LocalId};

    const _STABLE_ID_1: &str = "748540ca-b7c5-4c99-83ff-c1b8e02c09d6";
    const _STABLE_ID_2: &str = "0002c79e-b536-4776-b000-000266c252d5";

    fn initialize_compressor() -> (IdCompressor, Vec<f64>) {
        let mut compressor = IdCompressor::new(String::from(_STABLE_ID_1)).ok().unwrap();
        let mut generated_ids: Vec<f64> = Vec::new();
        for _ in 0..5 {
            generated_ids.push(compressor.generate_next_id());
        }
        (compressor, generated_ids)
    }

    fn finalize_compressor(compressor: &mut IdCompressor) {
        let interop_id_range = compressor.take_next_range();
        let InteropIds {
            first_local_gen_count,
            count,
        } = interop_id_range.unwrap();
        _ = compressor.finalize_range(
            String::from_utf8(compressor.get_local_session_id()).unwrap(),
            first_local_gen_count,
            count,
        )
    }

    #[test]
    #[should_panic]
    fn cluster_capacity_fract() {
        let (mut compressor, _) = initialize_compressor();
        _ = compressor.set_cluster_capacity(5.5);
    }

    #[test]
    #[should_panic]
    fn cluster_capacity_negative() {
        let (mut compressor, _) = initialize_compressor();
        _ = compressor.set_cluster_capacity(-2_f64);
    }

    #[test]
    fn generate_next_id() {
        let (mut compressor, generated_ids) = initialize_compressor();
        assert_eq!(
            compressor.generate_next_id(),
            generated_ids[generated_ids.len() - 1] - 1.0
        );
    }

    #[test]
    #[should_panic]
    fn get_token_invalid_uuid() {
        let (mut compressor, _) = initialize_compressor();
        _ = compressor.get_token(String::from("not_a_uuid"));
    }

    #[test]
    fn take_next_range() {
        let (mut compressor, generated_ids) = initialize_compressor();
        let interop_id_range = compressor.take_next_range();
        let InteropIds {
            first_local_gen_count,
            count,
        } = interop_id_range.unwrap();
        assert_eq!(
            get_id_from_local_id(LocalId::from_generation_count(first_local_gen_count as u64))
                as f64,
            generated_ids[0]
        );
        assert_eq!(count, generated_ids.len() as f64);
    }

    #[test]
    fn take_next_range_empty() {
        let mut compressor = IdCompressor::new(String::from(_STABLE_ID_1)).ok().unwrap();
        let interop_id_range = compressor.take_next_range();
        assert!(interop_id_range.is_none());
    }

    #[test]
    fn finalize_range() {
        let (mut compressor, _) = initialize_compressor();
        let interop_id_range = compressor.take_next_range();
        let InteropIds {
            first_local_gen_count,
            count,
        } = interop_id_range.unwrap();
        assert!(compressor
            .finalize_range(
                String::from_utf8(compressor.get_local_session_id()).unwrap(),
                first_local_gen_count,
                count
            )
            .is_ok());
    }

    #[test]
    fn normalize_to_op_space() {
        let (mut compressor, generated_ids) = initialize_compressor();
        finalize_compressor(&mut compressor);
        let id_count = generated_ids.len();
        for id in generated_ids {
            let op_space_id = compressor.normalize_to_op_space(id);
            assert_eq!(
                compressor.normalize_to_session_space(
                    op_space_id,
                    compressor
                        .compressor
                        .get_session_token_from_session_id(
                            compressor.compressor.get_local_session_id()
                        )
                        .ok()
                        .unwrap() as f64
                ),
                id
            );
        }
        let new_final = compressor.generate_next_id();
        assert_eq!(compressor.normalize_to_op_space(new_final), new_final);

        assert!(compressor
            .normalize_to_op_space(0.0 - (id_count as f64) - 1.0)
            .is_nan());
    }

    #[test]
    fn normalize_to_session_space() {
        let (mut compressor, _) = initialize_compressor();
        finalize_compressor(&mut compressor);
        assert_eq!(
            compressor.normalize_to_session_space(
                1.0,
                compressor
                    .compressor
                    .get_session_token_from_session_id(compressor.compressor.get_local_session_id())
                    .ok()
                    .unwrap() as f64
            ),
            -2_f64
        );
        assert!(compressor.normalize_to_session_space(1111.0, 0.0).is_nan());
    }

    #[test]
    #[should_panic]
    fn normalize_to_session_space_bad_token() {
        let (mut compressor, _) = initialize_compressor();
        finalize_compressor(&mut compressor);
        _ = compressor.normalize_to_session_space(-3_f64, -1.0);
    }

    #[test]
    fn decompress_invalid() {
        let (mut compressor, _) = initialize_compressor();

        assert!(compressor.decompress(1.0).is_none());
    }

    #[test]
    fn decompress() {
        let (mut compressor, generated_ids) = initialize_compressor();
        finalize_compressor(&mut compressor);
        let session_id = compressor.compressor.get_local_session_id();
        let base_stable = StableId::from(session_id);
        for id in generated_ids {
            let expected_offset = ((id * -1.0) - 1.0) as u64;
            let buff = compressor.decompress(id).unwrap();
            let uuid_str = String::from_utf8(buff).unwrap();
            assert_eq!(uuid_str, String::from(base_stable + expected_offset));
        }
    }

    #[test]
    fn recompress_invalid_uuid_string() {
        let (mut compressor, _) = initialize_compressor();

        assert!(compressor
            .recompress(String::from("invalid_uuid"))
            .is_none());
    }

    #[test]
    fn recompress_unknown_uuid() {
        let (mut compressor, _) = initialize_compressor();

        assert!(compressor.recompress(String::from(_STABLE_ID_2)).is_none());
    }

    #[test]
    fn recompress() {
        let (mut compressor, _) = initialize_compressor();
        finalize_compressor(&mut compressor);
        let session_id =
            (StableId::from(SessionId::from_uuid_string(_STABLE_ID_1).ok().unwrap()) + 1).into();
        assert!(compressor.recompress(session_id).is_some());
    }

    #[test]
    #[should_panic]
    fn deserialize_invalid() {
        let bytes: &[u8] = &[1, 2, 1, 0, 1];
        _ = IdCompressor::deserialize(bytes, String::from(_STABLE_ID_1));
    }

    #[test]
    fn serialize_deserialize() {
        let (mut compressor, _) = initialize_compressor();
        finalize_compressor(&mut compressor);
        _ = compressor.generate_next_id();
        let serialized_local = compressor.serialize(true);
        assert!(IdCompressor::deserialize(&serialized_local, String::from(_STABLE_ID_1)).is_ok());
        let serialized_final = compressor.serialize(false);
        assert!(IdCompressor::deserialize(&serialized_final, String::from(_STABLE_ID_2)).is_ok());
        let compressor_serialized_deserialized =
            IdCompressor::deserialize(&serialized_local, String::from(_STABLE_ID_1))
                .ok()
                .unwrap();
        assert!(compressor
            .compressor
            .equals_test_only(&compressor_serialized_deserialized.compressor, false))
    }

    #[test]
    fn test_nil_token() {
        let token = IdCompressor::get_nil_token();
        assert_eq!(token, -1 as f64);
    }

    #[cfg(feature = "debug-format")]
    #[test]
    fn test_pretty_print() {
        let (mut compressor, _) = initialize_compressor();
        finalize_compressor(&mut compressor);
        print!("{}", &compressor.to_debug_string());
    }
}
