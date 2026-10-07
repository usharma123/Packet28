import concurrent.futures
import json
from pathlib import Path
import subprocess

out = Path("archive-diagnostics")
out.mkdir(exist_ok=True)
p = subprocess.run(["cargo", "test", "-p", "packet28d", "--lib", "--all-features", "--locked", "--no-run", "--message-format=json"], stdout=subprocess.PIPE, text=True, check=True)
binary = next(json.loads(line)["executable"] for line in p.stdout.splitlines() if json.loads(line).get("executable"))
name = "task_archive::tests::admitted_requests_block_the_fence_and_the_fence_blocks_only_its_task"
def run(i):
    result = subprocess.run([binary, name, "--exact", "--nocapture"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=90)
    if result.returncode:
        (out / f"archive-{i}.log").write_text(result.stdout)
        print(result.stdout, flush=True)
    return result.returncode
with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
    codes = list(pool.map(run, range(300)))
print(f"Archive failures: {sum(code != 0 for code in codes)}/300", flush=True)
for i in range(5):
    p = subprocess.run([binary, "index::tests::", "--test-threads=16"], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=240)
    (out / f"daemon-{i}.log").write_text(p.stdout)
    print(p.stdout[p.stdout.rfind("\nfailures:"):] if p.returncode else f"Index round {i}: PASS", flush=True)
    codes.append(p.returncode)
raise SystemExit(int(any(codes)))
