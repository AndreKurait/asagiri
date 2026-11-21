#!/usr/bin/env python3
"""Validate the stdio server with the official MCP Python SDK."""
import asyncio
import json
import os
from pathlib import Path
import subprocess
import sys
import uuid

from mcp import ClientSession, StdioServerParameters, stdio_client


async def check(sock, image):
    if sys.platform == "win32":
        assert os.environ.get("GITHUB_ACTIONS") == "true" and sock == "npipe:////./pipe/asagiri-mcp-107"
        binary = os.environ["ASAGIRI_TEST_CLI"]
        local = Path(os.environ["RUNNER_TEMP"]) / "mcp-107-profile"
        local.mkdir(exist_ok=False)
        environment = dict(os.environ, LOCALAPPDATA=str(local), ASAGIRI_PIPE=r"\\.\pipe\asagiri-mcp-107")
        policy = local / "Asagiri/integrations/mcp.json"
    else:
        assert sys.platform == "linux" and os.geteuid() == 0 and Path("/.dockerenv").is_file()
        assert os.environ.get("ASAGIRI_TEST_ENGINE") == "1" and os.environ.get("ASAGIRI_DATA_DIR") == "/test-store"
        assert sock == "/run/asagiri/docker.sock"
        binary = "/opt/asagiri/asagiri"
        environment = dict(os.environ, ASAGIRI_SOCKET=sock)
        policy = Path.home() / ".asagiri/integrations/mcp.json"
    assert not policy.exists(), "Run against a fresh private integration configuration"
    container = None

    def command(args):
        return subprocess.check_output(args, env=environment, text=True, timeout=30).strip()

    def cli(*args):
        return json.loads(command([binary, "mcp", *args]))

    def docker(*args):
        endpoint = sock if sock.startswith("npipe://") else "unix://" + sock
        return command(["docker", "--host", endpoint, *args])

    try:
        assert cli("status") == {"enabled": False, "allowContainerChanges": False}
        params = StdioServerParameters(command=binary, args=["mcp"], env=environment)
        async with stdio_client(params) as (read, write):
            async with ClientSession(read, write, read_timeout_seconds=10) as session:
                hello = await session.initialize()
                assert hello.protocol_version == "2025-11-25" and hello.server_info.name == "asagiri"
                assert hello.capabilities.tools.list_changed
                assert (await session.list_tools()).tools == []
                cli("enable")
                tools = (await session.list_tools()).tools
                assert len(tools) == 7 and all(tool.annotations.read_only_hint for tool in tools)
                status = await session.call_tool("asagiri_status")
                assert not status.is_error and status.structured_content["version"]["AsagiriVersion"]
                assert json.loads(status.content[0].text) == status.structured_content
                secret = "SDK_REDACTION_" + uuid.uuid4().hex
                container = docker("create", "--label", "private=" + secret, "--env", "PRIVATE=" + secret,
                                   image, "sh", "-c", "echo official-sdk-ready; sleep 120")
                inspected = await session.call_tool("asagiri_inspect_container", {"container": container})
                assert not inspected.is_error and secret not in inspected.model_dump_json()
                denied = await session.call_tool("asagiri_start_container", {"container": container})
                assert denied.is_error
                cli("allow-changes", "on")
                assert len((await session.list_tools()).tools) == 10
                started = await session.call_tool("asagiri_start_container", {"container": container})
                assert not started.is_error and docker("inspect", "-f", "{{.State.Status}}", container) == "running"
                for _ in range(50):
                    logs = await session.call_tool("asagiri_container_logs", {"container": container})
                    assert not logs.is_error
                    if "official-sdk-ready" in logs.structured_content["logs"]:
                        break
                    await asyncio.sleep(0.1)
                else:
                    raise AssertionError("The SDK did not receive the container's output")
                stopped = await session.call_tool("asagiri_stop_container", {"container": container, "timeout": 0})
                assert not stopped.is_error
                cli("disable")
                assert (await session.list_tools()).tools == []
                revoked = await session.call_tool("asagiri_start_container", {"container": container})
                assert revoked.is_error and docker("inspect", "-f", "{{.State.Status}}", container) == "exited"
                await session.send_ping()
        print("PASS official MCP Python SDK 2.3.0: negotiation, schemas, read tools, redaction, actions and live revocation")
    finally:
        cli("disable")
        if container:
            docker("rm", "-f", container)
        policy.unlink(missing_ok=True)
        policy.with_name("mcp.lock").unlink(missing_ok=True)
        if policy.parent.exists():
            policy.parent.rmdir()


if __name__ == "__main__":
    asyncio.run(check(sys.argv[1], sys.argv[2] if len(sys.argv) > 2 else "alpine:3.21"))
