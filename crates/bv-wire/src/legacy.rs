//! Compatibility with the magics used before the project was renamed from
//! `smt-server`/`smt-wire` to `bv-server`/`bv-wire`.
//!
//! The legacy protocol is byte-for-byte identical to the current one apart from
//! the three 4-byte magics, so converting between them is a matter of rewriting
//! those magics in place: the request/response envelope magic and the magic of
//! the expression buffer embedded in a request or a simplify response.

use crate::constants::{
    response_flags, EXPR_MAGIC, REQUEST_ENVELOPE_LEN, REQUEST_MAGIC, RESPONSE_ENVELOPE_LEN,
    RESPONSE_MAGIC,
};

pub const LEGACY_EXPR_MAGIC: [u8; 4] = *b"SMT\0";
pub const LEGACY_REQUEST_MAGIC: [u8; 4] = *b"SMTQ";
pub const LEGACY_RESPONSE_MAGIC: [u8; 4] = *b"SMTR";

/// Byte offset of the embedded expression buffer in a request payload.
const REQUEST_EXPR_OFFSET: usize = REQUEST_ENVELOPE_LEN;
/// Byte offset of the `expr_len` field in a request envelope.
const REQUEST_EXPR_LEN_OFFSET: usize = 14;
/// Byte offset of the response flags in a response envelope.
const RESPONSE_FLAGS_OFFSET: usize = 9;
/// Byte offset of the embedded expression buffer in a simplify response
/// (after the envelope and the simplify block's `expr_len`/`target_node`).
const RESPONSE_EXPR_OFFSET: usize = RESPONSE_ENVELOPE_LEN + 8;

pub fn is_legacy_request_payload(payload: &[u8]) -> bool {
    payload.len() >= 4 && payload[..4] == LEGACY_REQUEST_MAGIC
}

pub fn is_legacy_response_payload(payload: &[u8]) -> bool {
    payload.len() >= 4 && payload[..4] == LEGACY_RESPONSE_MAGIC
}

/// Rewrite a legacy (`SMTQ`) request to current magics in place.
/// Returns `false` and leaves `payload` untouched if it is not a legacy request.
pub fn upgrade_request_in_place(payload: &mut [u8]) -> bool {
    if !is_legacy_request_payload(payload) {
        return false;
    }
    payload[..4].copy_from_slice(&REQUEST_MAGIC);
    let expr_len = payload
        .get(REQUEST_EXPR_LEN_OFFSET..REQUEST_EXPR_LEN_OFFSET + 4)
        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()) as usize)
        .unwrap_or(0);
    if expr_len >= 4 {
        rewrite_magic(
            payload,
            REQUEST_EXPR_OFFSET,
            &LEGACY_EXPR_MAGIC,
            &EXPR_MAGIC,
        );
    }
    true
}

/// Rewrite a legacy (`SMTR`) response to current magics in place.
/// Returns `false` and leaves `payload` untouched if it is not a legacy response.
pub fn upgrade_response_in_place(payload: &mut [u8]) -> bool {
    convert_response(
        payload,
        &LEGACY_RESPONSE_MAGIC,
        &RESPONSE_MAGIC,
        &LEGACY_EXPR_MAGIC,
        &EXPR_MAGIC,
    )
}

/// Rewrite a current (`BVSR`) response to legacy magics in place, for replying
/// to a client that sent a legacy request.
/// Returns `false` and leaves `payload` untouched if it is not a current response.
pub fn downgrade_response_in_place(payload: &mut [u8]) -> bool {
    convert_response(
        payload,
        &RESPONSE_MAGIC,
        &LEGACY_RESPONSE_MAGIC,
        &EXPR_MAGIC,
        &LEGACY_EXPR_MAGIC,
    )
}

fn convert_response(
    payload: &mut [u8],
    from: &[u8; 4],
    to: &[u8; 4],
    from_expr: &[u8; 4],
    to_expr: &[u8; 4],
) -> bool {
    if payload.len() < 4 || payload[..4] != *from {
        return false;
    }
    payload[..4].copy_from_slice(to);
    let has_expr = payload
        .get(RESPONSE_FLAGS_OFFSET)
        .is_some_and(|flags| flags & response_flags::HAS_EXPR != 0);
    if has_expr {
        rewrite_magic(payload, RESPONSE_EXPR_OFFSET, from_expr, to_expr);
    }
    true
}

fn rewrite_magic(payload: &mut [u8], offset: usize, from: &[u8; 4], to: &[u8; 4]) {
    if let Some(magic) = payload.get_mut(offset..offset + 4) {
        if magic == from {
            magic.copy_from_slice(to);
        }
    }
}
