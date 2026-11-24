"""Live tests for the optional Python Z3-only server."""
from __future__ import annotations

import pathlib
import socket
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

import bv_wire as bv  # noqa: E402
from bv_z3_server import Z3Server  # noqa: E402


def test_z3_server_uses_random_tcp_port() -> None:
    with Z3Server.start(port=0) as server:
        assert server.port != 0
        with socket.create_connection(server.address, timeout=5):
            pass


def test_z3_server_simple_sat_model_round_trip() -> None:
    with Z3Server.start(port=0) as server:
        ctx = bv.Context()
        x = ctx.bv_var("x", 4)
        ctx.assert_(ctx.bv_eq(x, 2))

        with server.client(timeout=5) as client:
            response = client.solve(ctx, request_id=0xA001)

        assert response.request_id == 0xA001
        assert response.status is bv.Status.SAT
        assert response.model is not None
        assert response.model[x].as_int() == 2


def test_z3_server_unsat_core_round_trip() -> None:
    with Z3Server.start(port=0) as server:
        ctx = bv.Context()
        x = ctx.bv_var("x", 4)
        ctx.assert_named("x_is_1", ctx.bv_eq(x, 1))
        ctx.assert_named("x_is_2", ctx.bv_eq(x, 2))

        with server.client(timeout=5) as client:
            response = client.solve(ctx, want_model=False, want_core=True)

        assert response.status is bv.Status.UNSAT
        assert response.core is not None
        assert set(response.core) == {"x_is_1", "x_is_2"}


def test_z3_server_assumptions_affect_solve() -> None:
    with Z3Server.start(port=0) as server:
        ctx = bv.Context()
        p = ctx.bool_var("p")
        ctx.assume(p)
        ctx.assert_(ctx.bool_not(p))

        with server.client(timeout=5) as client:
            response = client.solve(ctx, want_model=False)

        assert response.status is bv.Status.UNSAT


def test_z3_server_simplify_is_noop_but_protocol_compatible() -> None:
    with Z3Server.start(port=0) as server:
        ctx = bv.Context()
        x = ctx.bv_var("x", 8)
        target = x + 0

        with server.client(timeout=5) as client:
            result = client.simplify(target, request_id=0xA002)

        assert result.request_id == 0xA002
        assert result.status is bv.Status.SIMPLIFIED
        assert result.term is not None
        assert result.term.to_smt2(depth=-1) == "(bvadd x #x00) ; #2"


def test_z3_server_minimize_and_maximize_round_trip() -> None:
    with Z3Server.start(port=0) as server:
        ctx = bv.Context()
        x = ctx.bv_var("x", 4)
        ctx.assert_(ctx.bv_uge(x, 5))
        ctx.assert_(ctx.bv_ule(x, 9))

        with server.client(timeout=5) as client:
            minimum = client.minimize(x, want_model=True)
            maximum = client.maximize(x, want_model=False)

        assert minimum.status is bv.Status.SAT
        assert minimum.optimum is not None
        assert minimum.optimum.as_int() == 5
        assert minimum.model is not None
        assert minimum.model[x].as_int() == 5
        assert maximum.status is bv.Status.SAT
        assert maximum.optimum is not None
        assert maximum.optimum.as_int() == 9


def main() -> None:
    test_z3_server_uses_random_tcp_port()
    test_z3_server_simple_sat_model_round_trip()
    test_z3_server_unsat_core_round_trip()
    test_z3_server_assumptions_affect_solve()
    test_z3_server_simplify_is_noop_but_protocol_compatible()
    test_z3_server_minimize_and_maximize_round_trip()


if __name__ == "__main__":
    main()


def _legacy_request(payload: bytes) -> bytes:
    assert payload[:4] == bv.REQUEST_MAGIC and payload[32:36] == bv.EXPR_MAGIC
    return b"SMTQ" + payload[4:32] + b"SMT\0" + payload[36:]


def test_z3_server_answers_legacy_clients_with_legacy_magics() -> None:
    with Z3Server.start(port=0) as server:
        ctx = bv.Context()
        x = ctx.bv_var("x", 8)
        ctx.assert_(ctx.bv_eq(x, 3))

        solve = _legacy_request(ctx._build_solve_request(0xA010).payload)
        response = server._dispatch_payload(solve)
        assert response[:4] == b"SMTR"
        assert bv._parse_response(b"BVSR" + response[4:]).status is bv.Status.SAT

        simplify = _legacy_request(ctx._build_simplify_request(0xA011, x + 0).payload)
        response = server._dispatch_payload(simplify)
        assert response[:4] == b"SMTR"
        assert response[24:28] == b"SMT\0"
