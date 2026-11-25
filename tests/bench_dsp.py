"""GATE 1: бенчмарк JS vs Rust/WASM на одинаковом EQ-каскаде.

Поднимает локальный static-сервер на web/, открывает web/bench/index.html
в Chromium (обычный режим + 4x CPU-throttle как эмуляция мобильного CPU),
собирает JSON-отчёт и сохраняет web/bench/baseline.json.
Гейты: checksum_ok обязателен, страниц без ошибок, память не растёт.
Пороги скорости НЕ фиксированы — записываются как есть для решения primary.
"""
import json
import os
import socket
import sys
import threading
import time
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from tempfile import TemporaryDirectory
from urllib.request import urlopen

from playwright.sync_api import sync_playwright

ROOT = Path(__file__).resolve().parents[1]
RUNS = 3


def wait_server(origin):
    for _ in range(100):
        try:
            with urlopen(origin + "/bench/index.html", timeout=2) as r:
                if r.status == 200:
                    return
        except OSError:
            time.sleep(0.05)
    raise RuntimeError("static server did not start")


def run_once(browser, origin, throttle):
    context = browser.new_context(viewport={"width": 1280, "height": 800})
    page = context.new_page()
    errors = []
    page.on("pageerror", lambda e: errors.append(str(e)))
    if throttle and throttle != 1:
        cdp = context.new_cdp_session(page)
        cdp.send("Emulation.setCPUThrottlingRate", {"rate": throttle})
    page.goto(f"{origin}/bench/index.html?throttle={'cpu4x' if throttle != 1 else 'none'}")
    page.wait_for_function("window.__benchDone === true", timeout=120000)
    result = page.evaluate("window.__benchResult")
    context.close()
    assert not errors, errors
    assert "error" not in result, result.get("error")
    assert result["checksum_ok"], result["checksum_rel_diff"]
    assert not result["mem_grew"], (result["mem_bytes_before"], result["mem_bytes_after"])
    return result


def main():
    handler = partial(SimpleHTTPRequestHandler, directory=str(ROOT / "web"))
    httpd = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=httpd.serve_forever, daemon=True).start()
    origin = f"http://127.0.0.1:{httpd.server_port}"
    wait_server(origin)
    report = {"runs": [], "throttle_cpu4x": []}
    with sync_playwright() as driver:
        browser = driver.chromium.launch(args=["--enable-unsafe-swiftshader"])
        try:
            for _ in range(RUNS):
                report["runs"].append(run_once(browser, origin, 1))
            report["throttle_cpu4x"].append(run_once(browser, origin, 4))
        finally:
            browser.close()
    httpd.shutdown()
    httpd.server_close()
    out = ROOT / "web" / "bench" / "baseline.json"
    prev = None
    try:
        prev = json.loads(out.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        prev = None
    out.write_text(json.dumps(report, ensure_ascii=False, indent=1), encoding="utf-8")
    for key in ("runs", "throttle_cpu4x"):
        for run in report[key]:
            core = run["core"]
            print(key, "wasm/js ns_per_frame:",
                  round(core["wasm"]["ns_per_frame"]["p50"] / max(core["js"]["ns_per_frame"]["p50"], 1e-9), 3),
                  "xRT wasm:", round(core["wasm"]["xrt_median"], 1),
                  "checksum_rel:", run["checksum_rel_diff"])
    if isinstance(prev, dict):
        def med(runs, eng="wasm"):
            vals = [r["core"][eng]["ns_per_frame"]["p50"] for r in runs if "core" in r]
            vals.sort()
            return vals[len(vals) // 2] if vals else None
        for key in ("runs", "throttle_cpu4x"):
            old = med(prev.get(key, []))
            new = med(report.get(key, []))
            if old and new and new > 2 * old:
                print(f"WARNING regression {key}: p50 {old:.1f} -> {new:.1f} ns/frame (>2x baseline)")
    print("saved", out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
