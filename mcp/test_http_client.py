"""Check the MCP server built into the Rust Audacity app, using the official MCP Python SDK.

Usage (with Rust Audacity open):
    pip install mcp
    python mcp/test_http_client.py <token> [url]

The token is in the app under Agents ▸ MCP Server Settings & Setup… (Copy).
The default URL is http://127.0.0.1:8765/mcp. Nothing in your project is changed:
only read-only tools are called.
"""
import asyncio
import os
import sys

from mcp import ClientSession
from mcp.client.streamable_http import create_mcp_http_client, streamable_http_client

os.environ.setdefault("NO_PROXY", "127.0.0.1,localhost")
TOKEN = sys.argv[1] if len(sys.argv) > 1 else ""
URL = sys.argv[2] if len(sys.argv) > 2 else "http://127.0.0.1:8765/mcp"


async def main():
    headers = {"Authorization": f"Bearer {TOKEN}"} if TOKEN else {}
    async with streamable_http_client(URL, http_client=create_mcp_http_client(headers=headers)) as (r, w, *_):
        async with ClientSession(r, w) as s:
            init = await s.initialize()
            print("connected to", init.server_info.name, init.server_info.version, "· protocol", init.protocol_version)
            tools = (await s.list_tools()).tools
            print(len(tools), "tools:", ", ".join(t.name for t in tools))
            st = await s.call_tool("app_status", {})
            print("\napp_status:\n" + st.content[0].text)
            fx = await s.call_tool("list_effects", {})
            print("\nlist_effects:", "error" if fx.is_error else "ok")
            print("\nALL CHECKS PASSED")


def explain(e: BaseException) -> str:
    while isinstance(e, BaseExceptionGroup) and e.exceptions:
        e = e.exceptions[0]
    text = f"{type(e).__name__}: {e}"
    if "401" in text:
        return "the server refused the token (401). Copy it again from Agents ▸ MCP Server Settings & Setup…"
    if "Connect" in text or "refused" in text.lower():
        return "nothing is listening at " + URL + ". Is Rust Audacity open with the MCP server turned on?"
    return text


try:
    asyncio.run(main())
except BaseException as e:  # noqa: BLE001 – show one clear line instead of a traceback
    print("FAILED:", explain(e))
    sys.exit(1)
