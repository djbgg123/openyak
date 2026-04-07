from __future__ import annotations

import os

from openyak_sdk import OpenyakClient


def main() -> None:
    with OpenyakClient(base_url=os.environ["OPENYAK_BASE_URL"]) as client:
        thread = client.create_thread(
            model="claude-sonnet-4-6",
            allowed_tools=["read_file"],
        )
        paused = thread.run("PARITY_SCENARIO:request_user_input_roundtrip")
        if paused.status != "awaiting_user_input":
            raise RuntimeError(f"expected awaiting_user_input, got {paused.status}")

        resumed = thread.resume_user_input(
            request_id=paused.pending_user_input.request_id,
            content="feature",
            selected_option="feature",
        )
        print(resumed.status)
        print(resumed.final_text)


if __name__ == "__main__":
    main()
