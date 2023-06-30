/**
 * A compressed ID that has been normalized into "session space" (see `IdCompressor` for more).
 * Consumer-facing APIs and data structures should use session-space IDs as their lifetime and equality is stable and tied to
 * the scope of the session (i.e. compressor) that produced them.
 * @alpha
 */
export type SessionSpaceCompressedId = number & {
	readonly SessionUnique: "cea55054-6b82-4cbf-ad19-1fa645ea3b3e";
};

/**
 * A compressed ID that has been normalized into "op space".
 * Serialized/persisted structures (e.g. ops) should use op-space IDs as a performance optimization, as they require less normalizing when
 * received by a remote client due to the fact that op space for a given compressor is session space for all other compressors.
 */
export type OpSpaceCompressedId = number & {
	readonly OpNormalized: "9209432d-a959-4df7-b2ad-767ead4dbcae";
};

/**
 * A 128-bit Universally Unique IDentifier. Represented here
 * with a string of the form xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx,
 * where x is a lowercase hex digit.
 * @public
 */
export type UuidString = string & { readonly UuidString: "9d40d0ae-90d9-44b1-9482-9f55d59d5465" };

/**
 * A version 4, variant 1 uuid (https://datatracker.ietf.org/doc/html/rfc4122).
 */
export type StableId = UuidString & { readonly StableId: "53172b0d-a3d5-41ea-bd75-b43839c97f5a" };

/**
 * A StableId which is suitable for use as a session identifier
 */
export type SessionId = StableId & { readonly SessionId: "4498f850-e14e-4be9-8db0-89ec00997e58" };

/**
 * A StableId which is suitable for use as a session identifier
 */
export type NumericUuid = bigint & {
	readonly NumericUuid: "be04dd4d-9d7e-4337-a833-eec64c61aa46";
};
