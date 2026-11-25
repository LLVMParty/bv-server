//! Clients built before the smt-server -> bv-server rename send `SMTQ` requests
//! and expect `SMTR` responses with `SMT\0` expression buffers.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bv_server::{
    dispatch_payload_with_cache, serve_tcp, Backend, QueryResult, ResponseCache, ServerConfig,
};
use bv_wire::raw::constants::{EXPR_MAGIC, REQUEST_MAGIC, RESPONSE_MAGIC};
use bv_wire::raw::legacy::{
    upgrade_response_in_place, LEGACY_EXPR_MAGIC, LEGACY_REQUEST_MAGIC, LEGACY_RESPONSE_MAGIC,
};
use bv_wire::raw::{le, BinaryRequest, BinaryResponse, ExprBuilder, SimplifyBlock, Status};

/// Answers SOLVE with SAT and SIMPLIFY by echoing the request's expression.
struct EchoBackend {
    count: Arc<AtomicUsize>,
}

impl Backend for EchoBackend {
    fn name(&self) -> &'static str {
        "echo"
    }

    fn handle(&self, request: &BinaryRequest) -> bv_wire::Result<QueryResult> {
        self.count.fetch_add(1, Ordering::SeqCst);
        // Backends only ever see current magics.
        assert_eq!(request.expression[..4], EXPR_MAGIC);
        Ok(match request.target_ref() {
            Some(target_node) => QueryResult::simplified(SimplifyBlock {
                expression: request.expression.clone(),
                target_node,
            }),
            None => QueryResult::sat(None),
        })
    }
}

fn to_legacy_request(mut payload: Vec<u8>) -> Vec<u8> {
    assert_eq!(payload[..4], REQUEST_MAGIC);
    assert_eq!(payload[32..36], EXPR_MAGIC);
    payload[..4].copy_from_slice(&LEGACY_REQUEST_MAGIC);
    payload[32..36].copy_from_slice(&LEGACY_EXPR_MAGIC);
    payload
}

#[test]
fn legacy_solve_gets_legacy_response_and_shares_cache_with_current() {
    let count = Arc::new(AtomicUsize::new(0));
    let backend = EchoBackend {
        count: Arc::clone(&count),
    };
    let cache = ResponseCache::new();

    let mut builder = ExprBuilder::new();
    let t = builder.bool_true().unwrap();
    builder.assert(t).unwrap();
    let legacy = to_legacy_request(builder.build_solve_request(1, 0, false, false).unwrap());
    let current = builder.build_solve_request(2, 0, false, false).unwrap();

    let mut legacy_response = dispatch_payload_with_cache(&legacy, &backend, Some(&cache));
    assert_eq!(legacy_response[..4], LEGACY_RESPONSE_MAGIC);
    assert!(upgrade_response_in_place(&mut legacy_response));
    let legacy_response = BinaryResponse::parse(&legacy_response).unwrap();
    assert_eq!(legacy_response.envelope.status, Status::Sat);
    assert_eq!(legacy_response.envelope.request_id, 1);

    let current_response = dispatch_payload_with_cache(&current, &backend, Some(&cache));
    assert_eq!(current_response[..4], RESPONSE_MAGIC);
    let current_response = BinaryResponse::parse(&current_response).unwrap();
    assert_eq!(current_response.envelope.request_id, 2);

    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "second request is a cache hit"
    );
    assert_eq!(cache.len(), 1);
}

#[test]
fn legacy_simplify_response_uses_legacy_expression_magic() {
    let backend = EchoBackend {
        count: Arc::new(AtomicUsize::new(0)),
    };
    let cache = ResponseCache::new();

    let mut builder = ExprBuilder::new();
    let x = builder.bv_var("x", 8).unwrap();
    let request = builder.build_simplify_request(9, x).unwrap();

    // A cache miss for the legacy client, then a hit for each kind of client.
    for _ in 0..2 {
        let legacy = to_legacy_request(request.clone());
        let mut response = dispatch_payload_with_cache(&legacy, &backend, Some(&cache));
        assert_eq!(response[..4], LEGACY_RESPONSE_MAGIC);
        assert_eq!(response[24..28], LEGACY_EXPR_MAGIC);
        assert!(upgrade_response_in_place(&mut response));
        let response = BinaryResponse::parse(&response).unwrap();
        assert_eq!(response.envelope.status, Status::Simplified);
        assert_eq!(response.envelope.request_id, 9);

        let response = dispatch_payload_with_cache(&request, &backend, Some(&cache));
        assert_eq!(response[..4], RESPONSE_MAGIC);
        assert_eq!(response[24..28], EXPR_MAGIC);
        BinaryResponse::parse(&response).unwrap();
    }
}

#[test]
fn malformed_legacy_request_gets_legacy_error_response() {
    let backend = EchoBackend {
        count: Arc::new(AtomicUsize::new(0)),
    };
    let response = bv_server::dispatch_payload(b"SMTQ-truncated", &backend);
    assert_eq!(response[..4], LEGACY_RESPONSE_MAGIC);
}

#[test]
fn oversized_legacy_response_is_replaced_with_legacy_error() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    thread::spawn(move || {
        let mut config = ServerConfig::new(Arc::new(EchoBackend {
            count: Arc::new(AtomicUsize::new(0)),
        }));
        config.max_response_bytes = 15;
        let _ = serve_tcp(addr, config);
    });

    let mut builder = ExprBuilder::new();
    let t = builder.bool_true().unwrap();
    builder.assert(t).unwrap();
    let request = to_legacy_request(builder.build_solve_request(5, 0, false, false).unwrap());

    let mut stream = (0..100)
        .find_map(|_| {
            TcpStream::connect(addr)
                .map_err(|_| thread::sleep(Duration::from_millis(10)))
                .ok()
        })
        .expect("server did not start listening");
    stream
        .write_all(&le::encode_transport_frame(&request).unwrap())
        .unwrap();
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).unwrap();
    let mut response = vec![0u8; u32::from_le_bytes(len) as usize];
    stream.read_exact(&mut response).unwrap();

    assert_eq!(response[..4], LEGACY_RESPONSE_MAGIC);
    assert!(upgrade_response_in_place(&mut response));
    let response = BinaryResponse::parse(&response).unwrap();
    assert_eq!(response.envelope.status, Status::Error);
}
