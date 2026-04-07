from __future__ import annotations

import os

from openyak_sdk import OpenyakClient


def main() -> None:
    with OpenyakClient(base_url=os.environ["OPENYAK_BASE_URL"]) as client:
        thread = client.create_thread(
            model="claude-sonnet-4-6",
            allowed_tools=["bash"],
        )
        result = thread.run("PARITY_SCENARIO:bash_stdout_roundtrip")
        print(result.status)
        print(result.final_text)
        print(result.usage)


if __name__ == "__main__":
    main()
