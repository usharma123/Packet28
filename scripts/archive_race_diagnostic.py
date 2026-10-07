import subprocess
from pathlib import Path
out = Path("archive-diagnostics")
out.mkdir(exist_ok=True)
for i in range(3):
    command = ["cargo", "test", "--locked", "-p", "packet28d", "--test", "status_pagination", "seeded_five_thousand_task_daemon_keeps_status_live_and_pages_every_task", "--", "--exact", "--nocapture"]
    with (out / f"startup-{i}.log").open("w") as log:
        result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, timeout=600)
    print((out / f"startup-{i}.log").read_text(), flush=True)
    if result.returncode: raise SystemExit(result.returncode)
