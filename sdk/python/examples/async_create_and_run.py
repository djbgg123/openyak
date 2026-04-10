from __future__ import annotations

import asyncio
import os

from openyak_sdk import AsyncOpenyakClient


async def main() -> None:
    async with AsyncOpenyakClient(
        base_url=os.environ["OPENYAK_BASE_URL"],
        operator_token=os.environ.get("OPENYAK_OPERATOR_TOKEN"),
    ) as client:
        thread = await client.create_thread(
            model="claude-sonnet-4-6",
            allowed_tools=["bash"],
        )
        result = await thread.run("PARITY_SCENARIO:bash_stdout_roundtrip")
        print(result.status)
        print(result.final_text)
        print(result.usage)


if __name__ == "__main__":
    asyncio.run(main())
