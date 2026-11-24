"""Live end-to-end tests against the bv-server binary.

This intentionally uses only the standard library plus the local Python client
module so it can run directly in CI without pytest.
"""
from __future__ import annotations

import os
import pathlib
import socket
import subprocess
import sys
import time

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
PYTHON_ROOT = REPO_ROOT / "python"
sys.path.insert(0, str(PYTHON_ROOT))

import bv_wire as bv  # noqa: E402


def unused_local_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def server_binary() -> pathlib.Path:
    subprocess.check_call(["cargo", "build", "-p", "bv-server"], cwd=REPO_ROOT)
    exe = "bv-server.exe" if os.name == "nt" else "bv-server"
    return REPO_ROOT / "target" / "debug" / exe


def z3_library_dirs() -> list[str]:
    build_dir = REPO_ROOT / "target" / "debug" / "build"
    return [
        str(path)
        # Older Cargo: build/z3-sys-<hash>/out; newer: build/z3-sys/<hash>/out.
        for prefix in ("z3-sys-*", "z3-sys/*")
        for pattern in (f"{prefix}/out/z3-*/bin", f"{prefix}/out/z3-*/lib")
        for path in build_dir.glob(pattern)
    ]


def describe_z3_library_dirs() -> str:
    lines = []
    for directory in z3_library_dirs():
        names = sorted(p.name for p in pathlib.Path(directory).iterdir() if "z3" in p.name.lower())
        lines.append(f"  {directory}: {', '.join(names) or '(no z3 files)'}")
    return "\n".join(lines) or "  (none found)"


def server_environment() -> dict[str, str]:
    env = os.environ.copy()
    z3_dirs = z3_library_dirs()
    if z3_dirs:
        sep = os.pathsep
        env["PATH"] = sep.join(z3_dirs + [env.get("PATH", "")])
        env["LD_LIBRARY_PATH"] = sep.join(z3_dirs + [env.get("LD_LIBRARY_PATH", "")])
        env["DYLD_LIBRARY_PATH"] = sep.join(z3_dirs + [env.get("DYLD_LIBRARY_PATH", "")])
    return env


class LiveServer:
    def __init__(self) -> None:
        self.port = unused_local_port()
        self.proc: subprocess.Popen[bytes] | None = None

    def __enter__(self) -> "LiveServer":
        self.proc = subprocess.Popen(
            [str(server_binary()), f"127.0.0.1:{self.port}"],
            cwd=REPO_ROOT,
            env=server_environment(),
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )
        deadline = time.time() + 10.0
        last_error: OSError | None = None
        while time.time() < deadline:
            if self.proc.poll() is not None:
                stderr = self.proc.stderr.read().decode("utf-8", "replace") if self.proc.stderr else ""
                raise RuntimeError(
                    f"bv-server exited early with {self.proc.returncode}: {stderr}\n"
                    f"z3 library directories added to the server's search path:\n"
                    f"{describe_z3_library_dirs()}"
                )
            try:
                with socket.create_connection(("127.0.0.1", self.port), timeout=0.2):
                    return self
            except OSError as exc:
                last_error = exc
                time.sleep(0.05)
        raise TimeoutError(f"bv-server did not listen on port {self.port}: {last_error}")

    def __exit__(self, *exc_info: object) -> None:
        if self.proc is None:
            return
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=5)


def test_python_client_env_default_round_trip(port: int) -> None:
    old = os.environ.get(bv.SERVER_ADDRESS_ENV)
    os.environ[bv.SERVER_ADDRESS_ENV] = f"127.0.0.1:{port}"
    try:
        with bv.Client(timeout=5) as client:
            test_python_client_binary_round_trip(client)
    finally:
        if old is None:
            os.environ.pop(bv.SERVER_ADDRESS_ENV, None)
        else:
            os.environ[bv.SERVER_ADDRESS_ENV] = old


def test_python_client_binary_round_trip(client: bv.Client) -> None:
    ctx = bv.Context()
    x = ctx.bv_var("x", 4)
    ctx.assert_(ctx.bv_eq(x, 2))

    response = client.solve(ctx, request_id=0x1001)
    assert response.request_id == 0x1001
    assert response.status is bv.Status.SAT
    assert response.model is not None
    assert response.model[x].width == 4
    assert response.model[x].as_int() == 2


def test_binary_cache_rebinds_response_ids(client: bv.Client) -> None:
    ctx = bv.Context()
    x = ctx.bv_var("cached", 3)
    ctx.assert_(ctx.bv_eq(x, 5))

    first = client.solve(ctx, request_id=0x2001)
    second = client.solve(ctx, request_id=0x2002)
    assert first.request_id == 0x2001
    assert second.request_id == 0x2002
    assert first.status is bv.Status.SAT
    assert second.status is bv.Status.SAT


def test_python_client_optimization_round_trip(client: bv.Client) -> None:
    ctx = bv.Context()
    x = ctx.bv_var("opt", 4)
    ctx.assert_(ctx.bv_uge(x, 5))

    result = client.minimize(x, request_id=0x3001)
    assert result.request_id == 0x3001
    assert result.status is bv.Status.SAT
    assert result.optimum is not None
    assert result.optimum.width == 4
    assert result.optimum.as_int() == 5
    assert result.model is not None


def test_text_smtlib_round_trip(client: bv.Client) -> None:
    script = """
        #| yaspar parses this block comment in the live text path |#
        (set-logic QF_BV)
        (declare-const |x y| (_ BitVec 2))
        (assert (= |x y| #b11))
        (check-sat)
        (get-value (|x y|))
    """
    text = client.smt2(script)
    assert text.startswith("sat\n"), text
    assert "(|x y| #b11)" in text, text


def main() -> None:
    with LiveServer() as server:
        test_python_client_env_default_round_trip(server.port)
        with bv.Client("127.0.0.1", server.port, timeout=5) as client:
            test_python_client_binary_round_trip(client)
            test_binary_cache_rebinds_response_ids(client)
            test_python_client_optimization_round_trip(client)
            test_text_smtlib_round_trip(client)


if __name__ == "__main__":
    main()
